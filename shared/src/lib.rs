// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native shared-library delivery of [`implementation`].
//!
//! The implementation remains an rlib for host tests. This boundary exports its
//! Rust API without adding a second implementation or an FFI ownership contract.

// Keep one std owner across independent Rust DSOs.
extern crate hyper_rust_std as _;
// VM support uses the policy DSO rather than embedding another policy copy.
extern crate hyper_vm_policy_shared as _;

#[doc(no_inline)]
pub use implementation::*;
