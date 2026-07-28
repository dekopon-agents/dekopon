use std::{
    fs::File,
    io::{self, BufWriter},
    path::{Path, PathBuf},
};

use thiserror::Error;
use tracing_chrome::{ChromeLayerBuilder, FlushGuard};
use tracing_subscriber::{
    EnvFilter, Layer, fmt, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};

pub(crate) struct TraceGuard {
    _chrome: Option<FlushGuard>,
}

pub(crate) fn initialize(
    verbosity: u8,
    no_color: bool,
    trace: Option<&Path>,
) -> Result<TraceGuard, TraceError> {
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let stderr_layer = fmt::layer()
        .with_ansi(!no_color)
        .with_target(verbosity > 1)
        .with_writer(io::stderr)
        .with_filter(EnvFilter::new(level));

    if let Some(path) = trace {
        let file = File::create(path).map_err(|source| TraceError::Create {
            path: path.to_path_buf(),
            source,
        })?;
        let (chrome_layer, guard) = ChromeLayerBuilder::new()
            .writer(BufWriter::new(file))
            .include_args(true)
            .include_locations(true)
            .build();
        let chrome_layer = chrome_layer.with_filter(EnvFilter::new(
            "dekopon_run=trace,dekopon_provider_host=trace",
        ));
        tracing_subscriber::registry()
            .with(stderr_layer)
            .with(chrome_layer)
            .try_init()
            .map_err(|error| TraceError::Subscriber(error.to_string()))?;
        Ok(TraceGuard {
            _chrome: Some(guard),
        })
    } else {
        tracing_subscriber::registry()
            .with(stderr_layer)
            .try_init()
            .map_err(|error| TraceError::Subscriber(error.to_string()))?;
        Ok(TraceGuard { _chrome: None })
    }
}

#[derive(Debug, Error)]
pub(crate) enum TraceError {
    #[error("could not create trace file {}", path.display())]
    Create {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not install tracing subscriber: {0}")]
    Subscriber(String),
}
