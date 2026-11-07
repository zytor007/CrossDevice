use cd_geometry::{presets, MonitorInfo};
use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AirChoice {
    #[value(name = "13", alias = "13.6", alias = "air13", alias = "13-inch")]
    Thirteen,
    #[value(name = "15", alias = "15.3", alias = "air15", alias = "15-inch")]
    Fifteen,
    #[value(name = "both", alias = "all")]
    Both,
}

impl AirChoice {
    pub fn models(self) -> Vec<MonitorInfo> {
        match self {
            Self::Thirteen => vec![presets::macbook_air_13()],
            Self::Fifteen => vec![presets::macbook_air_15()],
            Self::Both => vec![presets::macbook_air_13(), presets::macbook_air_15()],
        }
    }
}
