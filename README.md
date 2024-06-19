# HDFS Native Object Store
An [object_store](https://docs.rs/object_store/latest/object_store/) implementation for HDFS based on the native Rust [hdfs-native](https://github.com/Kimahriman/hdfs-native) library.

# Compatibility
Each release supports a certain minor release of both the `object_store` crate and the underlying `hdfs-native` client.

|hdfs-native-object-store|object_store|hdfs-native|
|---|---|---|
|0.9.x|0.9|0.9|
|0.10.x|0.10|0.9|
|0.11.x|0.10|0.10|

# Usage
```rust
use hdfs_native_object_store::HdfsObjectStore;
let store = HdfsObjectStore::with_url("hdfs://localhost:9000")?;
```

# Documentation
See [Documentation](https://docs.rs/hdfs-native-object-store).