# HDFS Native Object Store
An [object_store](https://docs.rs/object_store/latest/object_store/) implementation for HDFS based on the native Rust [hdfs-native](https://github.com/Kimahriman/hdfs-native) library.

# Compatibility
Each release supports a certain minor release of both the `object_store` crate and the underlying `hdfs-native` client.

|hdfs-native-object-store|object_store|hdfs-native|
|---|---|---|
|0.9.x|0.9|0.9|
|0.10.x|0.10|0.9|
|0.11.x|0.10|0.10|
|0.12.x|>=0.10, <0.12|0.10|
|0.13.x|>=0.10, <0.12|0.11|
|0.14.x|0.12|0.11|
|0.15.x|>=0.12.2, <0.13|0.12|
|0.16.x|0.13|0.13|
|0.17.x|0.14|0.14|
|0.18.x|0.13 (`object-store-0-13`) or 0.14 (default)|0.15|

By default, this crate uses `object_store` 0.14. To use `object_store` 0.13, disable default features and enable `object-store-0-13`:

```toml
hdfs-native-object-store = { version = "0.18", default-features = false, features = ["object-store-0-13"] }
```

# Usage
```rust
use hdfs_native_object_store::HdfsObjectStoreBuilder;
let store = HdfsObjectStoreBuilder::new().with_url("hdfs://localhost:9000").build()?;
```

# Documentation
See [Documentation](https://docs.rs/hdfs-native-object-store).
