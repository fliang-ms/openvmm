// Copyright (C) Microsoft Corporation. All rights reserved.

//! Virtual PCI bus emulator, providing a PCI bus over a vmbus transport.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bus;
pub mod bus_control;
mod device;
mod protocol;
mod test_helpers;
