use crate::error::{Error, Result};
use ouisync_lib::{ConfigKey, ConfigStore, DeviceId};
use rand::{rngs::OsRng, Rng};
use std::io::ErrorKind;

const KEY: ConfigKey<DeviceId> = ConfigKey::new(
    "device_id",
    "The value stored in this file is the device ID. It is uniquelly generated for each device\n\
     and its only purpose is to detect when a database has been migrated from one device to\n\
     another.\n\
     \n\
     * When a database is migrated, the safest option is to NOT migrate this file with it. *\n\
     \n\
     However, the user may chose to *move* this file alongside the database. In such case it is\n\
     important to ensure the same device ID is never used by a writer replica concurrently from\n\
     more than one location. Doing so will likely result in data loss.\n\
     \n\
     Device ID is never used in construction of network messages and thus can't be used for peer\n\
     identification.",
);

pub async fn get_or_create(config: &ConfigStore) -> Result<DeviceId> {
    let cfg = config.entry(KEY);

    match cfg.get().await {
        Ok(id) => Ok(id),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let new_id = OsRng.gen();
            cfg.set(&new_id).await.map(|_| new_id)
        }
        Err(e) => Err(e),
    }
    .map_err(Error::Config)
}
