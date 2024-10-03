// Copyright (C) Microsoft Corporation. All rights reserved.

//! Schema definitions for JSON files generated by HvGuestState from a VMGSv1 file

use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;

pub const BIOS_LOADER_DEVICE_ID: &str = "ac6b8dc1-3257-4a70-b1b2-a9c9215659ad";

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Device {
    pub version: Version,
    pub r#type: String,
    pub states: HashMap<String, State>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum State {
    #[serde(rename_all = "PascalCase")]
    Nvram {
        vendors: HashMap<String, NvramVendor>,
        last_update_time: String,
    },
    Other(serde_json::Value),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct NvramVendor {
    pub variables: HashMap<String, NvramVariable>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct NvramVariable {
    pub attributes: u32,
    pub data: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RuntimeState {
    pub version: Version,
    pub devices: HashMap<String, Device>,
}
