# Object store implementation for the Native Rust HDFS client

## Usage

```rust
use hdfs_native_object_store::HdfsObjectStore;
use hdfs_native::Result;
fn main() -> Result<()> {
    let store = HdfsObjectStore::with_url("hdfs://localhost:9000")?;
    Ok(())
}
```