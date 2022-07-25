use std::{io, sync::Once};
use tracing_subscriber::EnvFilter;

pub(crate) struct Logger;

impl Logger {
    pub fn new() -> Result<Self, io::Error> {
        static LOG_INIT: Once = Once::new();
        LOG_INIT.call_once(|| {
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::from_default_env())
                .with_file(true)
                .with_line_number(true)
                .init()
        });

        Ok(Self)
    }
}
