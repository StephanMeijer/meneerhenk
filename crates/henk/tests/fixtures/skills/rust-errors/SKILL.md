---
name: rust-errors
description: Use when a change adds or changes error handling in Rust code.
license: MIT
metadata:
  owner: platform-team
---

# Error handling in Rust

- Library code returns typed errors (thiserror); only binaries use anyhow.
- No unwrap, expect or panic outside tests.
- An error message says what was being done and with which input.
