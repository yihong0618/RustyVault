//! This module is a Rust replica of
//! https://github.com/hashicorp/vault/blob/main/builtin/credential/approle/validation.go

use std::{
    collections::HashMap,
    time::{Duration, SystemTime},
};

use better_default::Default;
use openssl::{hash::MessageDigest, pkey::PKey, sign::Signer};
use serde::{Deserialize, Serialize};

use super::{AppRoleBackendInner, SECRET_ID_ACCESSOR_LOCAL_PREFIX, SECRET_ID_ACCESSOR_PREFIX, SECRET_ID_LOCAL_PREFIX};
use crate::{
    errors::RvError,
    modules::auth::expiration::MAX_LEASE_DURATION_SECS,
    storage::{Storage, StorageEntry},
    utils::{self, deserialize_duration, deserialize_system_time, serialize_duration, serialize_system_time},
};

const MAX_HMAC_INPUT_LENGTH: usize = 4096;

// secretIDStorageEntry represents the information stored in storage
// when a secret_id is created. The structure of the secret_id storage
// entry is the same for all the types of secret_ids generated.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecretIdStorageEntry {
    // Accessor for the secret_id. It is a random uuid serving as
    // a secondary index for the secret_id. This uniquely identifies
    // the secret_id it belongs to, and hence can be used for listing
    // and deleting secret_ids. Accessors cannot be used as valid
    // secret_id during login.
    pub secret_id_accessor: String,

    // Number of times this secret_id can be used to perform the login
    // operation
    pub secret_id_num_uses: i64,

    // Duration after which this secret_id should expire. This is capped by
    // the backend mount's max TTL value.
    #[serde(serialize_with = "serialize_duration", deserialize_with = "deserialize_duration")]
    pub secret_id_ttl: Duration,

    // The time when the secret_id was created
    #[serde(serialize_with = "serialize_system_time", deserialize_with = "deserialize_system_time")]
    #[default(SystemTime::now())]
    pub creation_time: SystemTime,

    // The time when the secret_id becomes eligible for tidy operation.
    // Tidying is performed by the PeriodicFunc of the backend which is 1
    // minute apart.
    #[serde(serialize_with = "serialize_system_time", deserialize_with = "deserialize_system_time")]
    #[default(SystemTime::now())]
    pub expiration_time: SystemTime,

    // The time representing the last time this storage entry was modified
    #[serde(serialize_with = "serialize_system_time", deserialize_with = "deserialize_system_time")]
    #[default(SystemTime::now())]
    pub last_updated_time: SystemTime,

    // metadata that belongs to the secret_id
    pub metadata: HashMap<String, String>,

    // cidr_list is a set of CIDR blocks that impose source address
    // restrictions on the usage of secret_id
    pub cidr_list: Vec<String>,

    // token_cidr_list is a set of CIDR blocks that impose source address
    // restrictions on the usage of the token generated by this secret_id
    pub token_cidr_list: Vec<String>,
}

// Represents the payload of the storage entry of the accessor that maps to a
// unique secret_id. Note that secret_id should never be stored in plaintext
// anywhere in the backend. secret_id_hmac will be used as an index to fetch the
// properties of the secret_id and to delete the secret_id.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecretIdAccessorStorageEntry {
    // Hash of the secret_id which can be used to find the storage index at which
    // properties of secret_id is stored.
    pub secret_id_hmac: String,
}

impl AppRoleBackendInner {
    // get_secret_id_storage_entry fetches the secret ID properties from physical
    // storage. The entry will be indexed based on the given HMACs of both role
    // name and the secret ID. This method will not acquire secret ID lock to fetch
    // the storage entry. Locks need to be acquired before calling this method.
    pub fn get_secret_id_storage_entry(
        &self,
        storage: &dyn Storage,
        role_secret_id_prefix: &str,
        role_name_hmac: &str,
        secret_id_hmac: &str,
    ) -> Result<Option<SecretIdStorageEntry>, RvError> {
        if secret_id_hmac == "" {
            return Err(RvError::ErrResponse("missing secret id hmac".to_string()));
        }

        if role_name_hmac == "" {
            return Err(RvError::ErrResponse("missing role name hmac".to_string()));
        }

        let entry_index = format!("{}{}/{}", role_secret_id_prefix, role_name_hmac, secret_id_hmac);
        let storage_entry = storage.get(&entry_index)?;
        if storage_entry.is_none() {
            return Ok(None);
        }

        let entry = storage_entry.unwrap();
        let ret: SecretIdStorageEntry = serde_json::from_slice(entry.value.as_slice())?;

        Ok(Some(ret))
    }

    // set_secret_id_storage_entry creates or updates a secret ID entry at the
    // physical storage. The entry will be indexed based on the given HMACs of both
    // role name and the secret ID. This method will not acquire secret ID lock to
    // create/update the storage entry. Locks need to be acquired before calling
    // this method.
    pub fn set_secret_id_storage_entry(
        &self,
        storage: &dyn Storage,
        role_secret_id_prefix: &str,
        role_name_hmac: &str,
        secret_id_hmac: &str,
        secret_entry: &SecretIdStorageEntry,
    ) -> Result<(), RvError> {
        if role_secret_id_prefix == "" {
            return Err(RvError::ErrResponse("missing secret id prefix".to_string()));
        }

        if secret_id_hmac == "" {
            return Err(RvError::ErrResponse("missing secret id hmac".to_string()));
        }

        if role_name_hmac == "" {
            return Err(RvError::ErrResponse("missing role name hmac".to_string()));
        }

        let entry_index = format!("{}{}/{}", role_secret_id_prefix, role_name_hmac, secret_id_hmac);
        let entry = StorageEntry::new(&entry_index, secret_entry)?;

        storage.put(&entry)
    }

    pub fn delete_secret_id_storage_entry(
        &self,
        storage: &dyn Storage,
        role_secret_id_prefix: &str,
        role_name_hmac: &str,
        secret_id_hmac: &str,
    ) -> Result<(), RvError> {
        if secret_id_hmac == "" {
            return Err(RvError::ErrResponse("missing secret id hmac".to_string()));
        }

        if role_name_hmac == "" {
            return Err(RvError::ErrResponse("missing role name hmac".to_string()));
        }

        let entry_index = format!("{}{}/{}", role_secret_id_prefix, role_name_hmac, secret_id_hmac);
        storage.delete(&entry_index)
    }

    // register_secret_id_entry creates a new storage entry for the given secret_id.
    pub fn register_secret_id_entry(
        &self,
        storage: &dyn Storage,
        role_name: &str,
        secret_id: &str,
        hmac_key: &str,
        role_secret_id_prefix: &str,
        secret_entry: &mut SecretIdStorageEntry,
    ) -> Result<(), RvError> {
        let role_name_hmac = create_hmac(hmac_key, role_name)?;
        let secret_id_hmac = create_hmac(hmac_key, secret_id)?;

        let lock_entry = self.secret_id_locks.get_lock(&secret_id_hmac);
        {
            let _locked = lock_entry.lock.read()?;

            let entry =
                self.get_secret_id_storage_entry(storage, role_secret_id_prefix, &role_name_hmac, &secret_id_hmac)?;
            if entry.is_some() {
                return Err(RvError::ErrResponse("secret_id is already registered".to_string()));
            }
        }
        {
            let _locked = lock_entry.lock.write()?;

            let entry =
                self.get_secret_id_storage_entry(storage, role_secret_id_prefix, &role_name_hmac, &secret_id_hmac)?;
            if entry.is_some() {
                return Err(RvError::ErrResponse("secret_id is already registered".to_string()));
            }

            let now = SystemTime::now();
            secret_entry.creation_time = now;
            secret_entry.last_updated_time = now;

            let ttl = self.derive_secret_id_ttl(secret_entry.secret_id_ttl);
            if ttl.as_secs() != 0 {
                secret_entry.expiration_time = now + ttl;
            }

            self.create_secret_id_accessor_entry(storage, secret_entry, &secret_id_hmac, &role_secret_id_prefix)?;

            self.set_secret_id_storage_entry(
                storage,
                role_secret_id_prefix,
                &role_name_hmac,
                &secret_id_hmac,
                secret_entry,
            )?;
            Ok(())
        }
    }

    // derive_secret_id_ttl determines the secret id TTL to use based on the system's
    // max lease TTL.
    //
    // If secret_id_ttl is negative or if it crosses the backend mount's limit,
    // return to backend's max lease TTL. Otherwise, return the provided secret_id_ttl
    // value.
    pub fn derive_secret_id_ttl(&self, secret_id_ttl: Duration) -> Duration {
        if secret_id_ttl > MAX_LEASE_DURATION_SECS {
            return MAX_LEASE_DURATION_SECS;
        }

        secret_id_ttl
    }

    // secret_id_accessor_entry is used to read the storage entry that maps an
    // accessor to a secret_id.
    pub fn get_secret_id_accessor_entry(
        &self,
        storage: &dyn Storage,
        secret_id_accessor: &str,
        role_secret_id_prefix: &str,
    ) -> Result<Option<SecretIdAccessorStorageEntry>, RvError> {
        if secret_id_accessor == "" {
            return Err(RvError::ErrResponse("missing secret id accessor".to_string()));
        }

        let salt = self.salt.read()?;
        if salt.is_none() {
            return Err(RvError::ErrResponse("approle module not initialized".to_string()));
        }

        let salt_id = salt.as_ref().unwrap().salt_id(secret_id_accessor)?;

        let mut accessor_prefix = SECRET_ID_ACCESSOR_PREFIX;
        if role_secret_id_prefix == SECRET_ID_LOCAL_PREFIX {
            accessor_prefix = SECRET_ID_ACCESSOR_LOCAL_PREFIX;
        }

        let entry_index = format!("{}{}", accessor_prefix, salt_id);

        let lock_entry = self.secret_id_accessor_locks.get_lock(&secret_id_accessor);
        let _locked = lock_entry.lock.read()?;

        let storage_entry = storage.get(&entry_index)?;
        if storage_entry.is_none() {
            return Ok(None);
        }

        let entry = storage_entry.unwrap();
        let ret: SecretIdAccessorStorageEntry = serde_json::from_slice(entry.value.as_slice())?;

        Ok(Some(ret))
    }

    // create_secret_id_accessor_entry creates an identifier for the secret_id.
    // A storage index, mapping the accessor to the secret_id is also created.
    // This method should be called when the lock for the corresponding secret_id is held.
    pub fn create_secret_id_accessor_entry(
        &self,
        storage: &dyn Storage,
        entry: &mut SecretIdStorageEntry,
        secret_id_hmac: &str,
        role_secret_id_prefix: &str,
    ) -> Result<(), RvError> {
        entry.secret_id_accessor = utils::generate_uuid();

        let salt = self.salt.read()?;
        if salt.is_none() {
            return Err(RvError::ErrResponse("approle module not initialized".to_string()));
        }

        let salt_id = salt.as_ref().unwrap().salt_id(&entry.secret_id_accessor)?;

        let mut accessor_prefix = SECRET_ID_ACCESSOR_PREFIX;
        if role_secret_id_prefix == SECRET_ID_LOCAL_PREFIX {
            accessor_prefix = SECRET_ID_ACCESSOR_LOCAL_PREFIX;
        }

        let entry_index = format!("{}{}", accessor_prefix, salt_id);

        let lock_entry = self.secret_id_accessor_locks.get_lock(&entry.secret_id_accessor);
        let _locked = lock_entry.lock.write()?;

        let entry = StorageEntry::new(
            &entry_index,
            &SecretIdAccessorStorageEntry { secret_id_hmac: secret_id_hmac.to_string() },
        )?;

        storage.put(&entry)
    }

    // delete_secret_id_accessor_entry deletes the storage index mapping the accessor to a secret_id.
    pub fn delete_secret_id_accessor_entry(
        &self,
        storage: &dyn Storage,
        secret_id_accessor: &str,
        role_secret_id_prefix: &str,
    ) -> Result<(), RvError> {
        let salt = self.salt.read()?;
        if salt.is_none() {
            return Err(RvError::ErrResponse("approle module not initialized".to_string()));
        }

        let salt_id = salt.as_ref().unwrap().salt_id(secret_id_accessor)?;

        let mut accessor_prefix = SECRET_ID_ACCESSOR_PREFIX;
        if role_secret_id_prefix == SECRET_ID_LOCAL_PREFIX {
            accessor_prefix = SECRET_ID_ACCESSOR_LOCAL_PREFIX;
        }

        let entry_index = format!("{}{}", accessor_prefix, salt_id);

        let lock_entry = self.secret_id_accessor_locks.get_lock(secret_id_accessor);
        let _locked = lock_entry.lock.write()?;

        storage.delete(&entry_index)
    }

    // flush_role_secrets deletes all the secret_id that belong to the given
    // role_id.
    pub fn flush_role_secrets(
        &self,
        storage: &dyn Storage,
        role_name: &str,
        hmac_key: &str,
        role_secret_id_prefix: &str,
    ) -> Result<(), RvError> {
        let role_name_hmac = create_hmac(hmac_key, role_name)?;
        let key = format!("{}{}/", role_secret_id_prefix, role_name_hmac);
        let secret_id_hmacs = storage.list(&key)?;
        for secret_id_hmac in secret_id_hmacs.iter() {
            let entry_index = format!("{}{}/{}", role_secret_id_prefix, role_name_hmac, secret_id_hmac);
            let lock_entry = self.secret_id_locks.get_lock(&secret_id_hmac);
            let _locked = lock_entry.lock.write()?;
            storage.delete(&entry_index)?
        }

        Ok(())
    }
}

pub fn create_hmac(key: &str, value: &str) -> Result<String, RvError> {
    if key == "" {
        return Err(RvError::ErrResponse("invalid hmac key".to_string()));
    }

    if value.len() > MAX_HMAC_INPUT_LENGTH {
        return Err(RvError::ErrResponse(format!("value is longer than maximum of {} bytes", MAX_HMAC_INPUT_LENGTH)));
    }

    let pkey = PKey::hmac(key.as_bytes())?;
    let mut signer = Signer::new(MessageDigest::sha256(), &pkey)?;
    signer.update(value.as_bytes())?;
    let hmac = signer.sign_to_vec()?;
    Ok(hex::encode(hmac.as_slice()))
}

pub fn verify_cidr_role_secret_id_subset(
    secret_id_cidrs: &[String],
    role_bound_cidr_list: &[String],
) -> Result<(), RvError> {
    if !secret_id_cidrs.is_empty() && !role_bound_cidr_list.is_empty() {
        let cidr_list: Vec<String> = role_bound_cidr_list
            .iter()
            .map(|cidr| if cidr.contains("/") { cidr.clone() } else { format!("{}/32", cidr) })
            .collect();

        let cidr_list_ref: Vec<&str> = cidr_list.iter().map(String::as_str).collect();
        let cidrs_ref: Vec<&str> = secret_id_cidrs.iter().map(AsRef::as_ref).collect();

        if !utils::cidr::subset_blocks(&cidr_list_ref, &cidrs_ref)? {
            return Err(RvError::ErrResponse(format!(
                "failed to verify subset relationship between CIDR blocks on the role {:?} and CIDR blocks on the \
                 secret ID {:?}",
                cidr_list_ref, cidrs_ref
            )));
        }
    }

    Ok(())
}
