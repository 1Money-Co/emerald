use core::str::FromStr;
use clap::Parser;
use malachitebft_config::{BootstrapProtocol, RuntimeConfig, Selector, TransportProtocol};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeFlavour {
    SingleThreaded,
    MultiThreaded(usize),
}

impl FromStr for RuntimeFlavour {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(("multi-threaded", n)) = s.split_once(':') {
            return Ok(Self::MultiThreaded(
                n.parse().map_err(|_| format!("invalid thread count: {n}"))?,
            ));
        }
        match s {
            "single-threaded" => Ok(Self::SingleThreaded),
            "multi-threaded" => Ok(Self::MultiThreaded(0)),
            _ => Err(format!("unknown runtime flavour: {s}")),
        }
    }
}

impl RuntimeFlavour {
    pub fn to_runtime_config(self) -> RuntimeConfig {
        match self {
            Self::SingleThreaded => RuntimeConfig::SingleThreaded,
            Self::MultiThreaded(n) => RuntimeConfig::MultiThreaded { worker_threads: n },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum KeyProviderType {
    File,
    AwsSmKms,
}

#[derive(Parser, Debug, Clone, PartialEq)]
pub struct P2pOptions {
    #[clap(short, long, default_value = "single-threaded", verbatim_doc_comment)]
    pub runtime: RuntimeFlavour,
    #[clap(long, default_value = "false")]
    pub enable_discovery: bool,
    #[clap(long, default_value = "full")]
    pub bootstrap_protocol: BootstrapProtocol,
    #[clap(long, default_value = "random")]
    pub selector: Selector,
    #[clap(long, default_value = "20")]
    pub num_outbound_peers: usize,
    #[clap(long, default_value = "20")]
    pub num_inbound_peers: usize,
    #[clap(long, default_value = "5000")]
    pub ephemeral_connection_timeout_ms: u64,
    #[clap(short, long, default_value = "tcp")]
    pub transport: TransportProtocol,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_threaded_with_count() {
        let f: RuntimeFlavour = "multi-threaded:4".parse().unwrap();
        assert_eq!(f, RuntimeFlavour::MultiThreaded(4));
    }

    #[test]
    fn parses_single_threaded() {
        let f: RuntimeFlavour = "single-threaded".parse().unwrap();
        assert_eq!(f, RuntimeFlavour::SingleThreaded);
    }

    #[test]
    fn rejects_invalid_flavour() {
        assert!("nonsense".parse::<RuntimeFlavour>().is_err());
    }
}
