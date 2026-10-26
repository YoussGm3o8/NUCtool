use crate::model::Result;
use serde::{Deserialize, Serialize};
use wmi::WMIConnection;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Cpu {
    pub name: String,
    pub number_of_cores: Option<u32>,
    pub number_of_logical_processors: Option<u32>,
    pub l3_cache_size: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Machine {
    #[serde(skip_serializing)]
    pub identity: String,
    pub model: String,
    pub bios: String,
    pub cpu: Cpu,
    pub gpus: Vec<String>,
    pub services: Vec<Service>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Service {
    pub name: String,
    pub state: String,
}

#[derive(Deserialize)]
struct Product {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "UUID")]
    uuid: String,
}
#[derive(Deserialize)]
struct Bios {
    #[serde(rename = "SMBIOSBIOSVersion")]
    version: String,
}
#[derive(Deserialize)]
struct Video {
    #[serde(rename = "Name")]
    name: Option<String>,
}

impl Machine {
    pub fn read() -> Result<Self> {
        let connection = WMIConnection::new().map_err(|e| e.to_string())?;
        let product = connection
            .raw_query::<Product>("SELECT Name, UUID FROM Win32_ComputerSystemProduct")
            .map_err(|e| e.to_string())?
            .into_iter()
            .next()
            .ok_or("No computer identity returned.")?;
        let model = product.name;
        let bios = connection
            .raw_query::<Bios>("SELECT SMBIOSBIOSVersion FROM Win32_BIOS")
            .map_err(|e| e.to_string())?
            .into_iter()
            .next()
            .ok_or("No BIOS identity returned.")?
            .version;
        let cpu = connection.raw_query::<Cpu>("SELECT Name, NumberOfCores, NumberOfLogicalProcessors, L3CacheSize FROM Win32_Processor").map_err(|e| e.to_string())?.into_iter().next().ok_or("No CPU returned.")?;
        let services = connection.raw_query::<Service>("SELECT Name, State FROM Win32_Service WHERE Name = 'UniwillService' OR Name = 'NucSoftwareStudioService'").map_err(|e| e.to_string())?;
        let gpus = connection
            .raw_query::<Video>("SELECT Name FROM Win32_VideoController")
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter_map(|v| v.name)
            .collect();
        let identity = format!("{}|{}|{}", model, bios, product.uuid);
        Ok(Self {
            identity,
            model,
            bios,
            cpu,
            gpus,
            services,
        })
    }

    pub fn supports_ec(&self) -> bool {
        self.model == "LAPKC71E"
            && self.bios.starts_with("KCTGL357.")
            && self.cpu.name.contains("i7-11800H")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn missing_specs_are_unavailable_instead_of_guessed() {
        let cpu: super::Cpu = serde_json::from_str(r#"{"Name":"Detected CPU","NumberOfCores":4,"NumberOfLogicalProcessors":8,"L3CacheSize":null}"#).unwrap();
        assert_eq!(cpu.number_of_cores, Some(4));
        assert_eq!(cpu.l3_cache_size, None);
    }
}
