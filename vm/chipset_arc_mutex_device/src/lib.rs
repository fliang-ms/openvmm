// Copyright (C) Microsoft Corporation. All rights reserved.

//! VMM-agnostic infrastructure to wire up
//! [`ChipsetDevice`](chipset_device::ChipsetDevice) instances using
//! `Arc<CloseableMutex<dyn ChipsetDevice>>` to communicate with the backing chipset.
//!
//! NOTE: this crate is no longer used by HvLite/Underhill, and only remains
//! in-tree to support testing devices.

#![warn(missing_docs)]

pub mod device;
pub mod services;

pub mod test_chipset;
