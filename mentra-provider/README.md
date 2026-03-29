# mentra-provider

`mentra-provider` is Mentra's publishable provider-core crate.

It contains provider-neutral request, response, model, streaming, and tool
schema types that can be reused without depending on the full Mentra runtime.

For most application code, depend on
[`mentra`](https://crates.io/crates/mentra) instead. The `mentra` crate
re-exports these provider-core types and adds the runtime, tooling,
persistence, and collaboration layers.
