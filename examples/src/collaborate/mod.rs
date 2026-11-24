// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! E-commerce collaborative task example.
//!
//! Three specialist agents (orders, inventory, shipping) with hardcoded
//! domain data, mirroring the Go reference implementation.
//!
//! # Running the example
//!
//! Start each agent in a separate terminal:
//! ```text
//! cargo run --bin collaborate-orders    --package examples
//! cargo run --bin collaborate-inventory --package examples
//! cargo run --bin collaborate-shipping  --package examples
//! ```
//!
//! Then use the CLI:
//! ```text
//! a2acli collaborate \
//!   --agent http://localhost:9001 \
//!   --agent http://localhost:9002 \
//!   --agent http://localhost:9003 \
//!   "Customer ORD-1001 has not received their order — what is going on?"
//! ```

pub mod agent;
pub mod fan_in;
pub mod server;
