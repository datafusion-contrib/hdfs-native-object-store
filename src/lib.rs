//! [object_store::ObjectStore] implementation for the Native Rust HDFS client
//!
//! # Usage
//!
//! ```rust
//! use hdfs_native_object_store::HdfsObjectStoreBuilder;
//! let store = HdfsObjectStoreBuilder::new()
//!     .with_url("hdfs://localhost:9000")
//!     .build()
//!     .unwrap();
//! ```
//!
use std::{
    collections::HashMap,
    fmt::{Display, Formatter},
    future,
    path::PathBuf,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::{
    FutureExt,
    stream::{BoxStream, StreamExt},
};
use hdfs_native::{
    Client, ClientBuilder, HdfsError, WriteOptions, client::FileStatus, file::FileWriter,
};
use object_store::{CopyMode, CopyOptions, RenameOptions, RenameTargetMode};
#[allow(deprecated)]
use object_store::{
    GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMode, PutMultipartOpts, PutOptions, PutPayload, PutResult, Result, UploadPart, path::Path,
};
use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot},
    task::{self, JoinHandle},
};

// Re-export minidfs for down-stream integration tests
#[cfg(feature = "integration-test")]
pub use hdfs_native::minidfs;

fn generic_error(
    source: Box<dyn std::error::Error + Send + Sync + 'static>,
) -> object_store::Error {
    object_store::Error::Generic {
        store: "HFDS",
        source,
    }
}

/// Builder for creating an [HdfsObjectStore]
#[derive(Default)]
pub struct HdfsObjectStoreBuilder {
    inner: ClientBuilder,
}

impl HdfsObjectStoreBuilder {
    /// Create a new [HdfsObjectStoreBuilder]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the URL to connect to. Can be the address of a single NameNode, or a logical NameService
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.inner = self.inner.with_url(url);
        self
    }

    /// Set configs to use for the client. The provided configs will override any found in the default config files loaded
    pub fn with_config(
        mut self,
        config: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        self.inner = self.inner.with_config(config);
        self
    }

    // Use a dedicated tokio runtime for spawned tasks and IO operations
    pub fn with_io_runtime(mut self, runtime: Handle) -> Self {
        self.inner = self.inner.with_io_runtime(runtime);
        self
    }

    /// Create the [HdfsObjectStore]] instance from the provided settings
    pub fn build(self) -> Result<HdfsObjectStore> {
        let client = self.inner.build().to_object_store_err()?;

        Ok(HdfsObjectStore { client })
    }
}

/// Interface for [Hadoop Distributed File System](https://hadoop.apache.org/docs/stable/hadoop-project-dist/hadoop-hdfs/HdfsDesign.html).
#[derive(Debug, Clone)]
pub struct HdfsObjectStore {
    client: Client,
}

impl HdfsObjectStore {
    /// Creates a new HdfsObjectStore from an existing `hdfs-native` [Client]
    ///
    /// ```rust
    /// # use std::sync::Arc;
    /// use hdfs_native::ClientBuilder;
    /// # use hdfs_native_object_store::HdfsObjectStore;
    /// let client = ClientBuilder::new().with_url("hdfs://127.0.0.1:9000").build().unwrap();
    /// let store = HdfsObjectStore::new(client);
    /// ```
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// Creates a new HdfsObjectStore using the specified URL
    ///
    /// Connect to a NameNode
    /// ```rust
    /// # use hdfs_native_object_store::HdfsObjectStore;
    /// # fn main() -> object_store::Result<()> {
    /// let store = HdfsObjectStore::with_url("hdfs://127.0.0.1:9000")?;
    /// # Ok(())
    /// # }
    /// ```
    #[deprecated(since = "0.15.0", note = "Use HdfsObjectStoreBuilder instead")]
    pub fn with_url(url: &str) -> Result<Self> {
        let client = ClientBuilder::new()
            .with_url(url)
            .build()
            .to_object_store_err()?;

        Ok(Self { client })
    }

    /// Creates a new HdfsObjectStore using the specified URL and Hadoop configs.
    ///
    /// Connect to a NameService
    /// ```rust
    /// # use hdfs_native_object_store::HdfsObjectStore;
    /// # use std::collections::HashMap;
    /// # fn main() -> object_store::Result<()> {
    /// let config = HashMap::from([
    ///     ("dfs.ha.namenodes.ns".to_string(), "nn1,nn2".to_string()),
    ///     ("dfs.namenode.rpc-address.ns.nn1".to_string(), "nn1.example.com:9000".to_string()),
    ///     ("dfs.namenode.rpc-address.ns.nn2".to_string(), "nn2.example.com:9000".to_string()),
    /// ]);
    /// let store = HdfsObjectStore::with_config("hdfs://ns", config)?;
    /// # Ok(())
    /// # }
    /// ```
    #[deprecated(since = "0.15.0", note = "Use HdfsObjectStoreBuilder instead")]
    pub fn with_config(url: &str, config: HashMap<String, String>) -> Result<Self> {
        let client = ClientBuilder::new()
            .with_url(url)
            .with_config(config)
            .build()
            .to_object_store_err()?;

        Ok(Self { client })
    }

    async fn open_tmp_file(&self, file_path: &str) -> Result<(FileWriter, String)> {
        let path_buf = PathBuf::from(file_path);

        let file_name = path_buf
            .file_name()
            .ok_or(HdfsError::InvalidPath("path missing filename".to_string()))
            .to_object_store_err()?
            .to_str()
            .ok_or(HdfsError::InvalidPath("path not valid unicode".to_string()))
            .to_object_store_err()?
            .to_string();

        let tmp_file_path = path_buf
            .with_file_name(format!(".{file_name}.tmp"))
            .to_str()
            .ok_or(HdfsError::InvalidPath("path not valid unicode".to_string()))
            .to_object_store_err()?
            .to_string();

        // Try to create a file with an incrementing index until we find one that doesn't exist yet
        let mut index = 1;
        loop {
            let path = format!("{tmp_file_path}.{index}");
            match self.client.create(&path, WriteOptions::default()).await {
                Ok(writer) => break Ok((writer, path)),
                Err(HdfsError::AlreadyExists(_)) => index += 1,
                Err(e) => break Err(e).to_object_store_err(),
            }
        }
    }
}

impl Display for HdfsObjectStore {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "HdfsObjectStore")
    }
}

impl From<Client> for HdfsObjectStore {
    fn from(value: Client) -> Self {
        Self { client: value }
    }
}

#[async_trait]
impl ObjectStore for HdfsObjectStore {
    /// Save the provided bytes to the specified location
    ///
    /// To make the operation atomic, we write to a temporary file `.{filename}.tmp.{i}` and rename
    /// on a successful write, where `i` is an integer that is incremented until a non-existent file
    /// is found.
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Result<PutResult> {
        let overwrite = match opts.mode {
            PutMode::Create => false,
            PutMode::Overwrite => true,
            PutMode::Update(_) => {
                return Err(object_store::Error::NotImplemented {
                    operation: "PutOptions with Update precondition".to_string(),
                    implementer: "HdfsObjectStore".to_string(),
                });
            }
        };

        let final_file_path = make_absolute_file(location);

        // If we're not overwriting, do an upfront check to see if the file already
        // exists. Otherwise we have to write the whole file and try to rename before
        // finding out.
        if !overwrite && self.client.get_file_info(&final_file_path).await.is_ok() {
            return Err(HdfsError::AlreadyExists(final_file_path)).to_object_store_err();
        }

        let (mut tmp_file, tmp_file_path) = self.open_tmp_file(&final_file_path).await?;

        for bytes in payload {
            tmp_file.write(bytes).await.to_object_store_err()?;
        }
        tmp_file.close().await.to_object_store_err()?;

        self.client
            .rename(&tmp_file_path, &final_file_path, overwrite)
            .await
            .to_object_store_err()?;

        let e_tag = self
            .get_opts(location, GetOptions::default().with_head(true))
            .await?
            .meta
            .e_tag;

        Ok(PutResult {
            e_tag,
            version: None,
        })
    }

    /// Create a multipart writer that writes to a temporary file in a background task, and renames
    /// to the final destination on complete.
    #[allow(deprecated)]
    async fn put_multipart_opts(
        &self,
        location: &Path,
        _opts: PutMultipartOpts,
    ) -> Result<Box<dyn MultipartUpload>> {
        let final_file_path = make_absolute_file(location);

        let (tmp_file, tmp_file_path) = self.open_tmp_file(&final_file_path).await?;

        Ok(Box::new(HdfsMultipartWriter::new(
            self.client.clone(),
            tmp_file,
            &tmp_file_path,
            &final_file_path,
        )))
    }

    /// Reads data for the specified location.
    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        let status = self
            .client
            .get_file_info(&make_absolute_file(location))
            .await
            .to_object_store_err()?;

        if status.isdir {
            return Err(object_store::Error::NotFound {
                path: location.to_string(),
                source: "Head cannot be called on a directory".into(),
            });
        }

        let meta = get_object_meta(&status)?;

        options.check_preconditions(&meta)?;

        let (range, payload) = if options.head {
            (
                (0..0),
                GetResultPayload::Stream(futures::stream::empty().boxed()),
            )
        } else {
            let range = options
                .range
                .map(|r| r.as_range(meta.size))
                .transpose()
                .map_err(|source| generic_error(source.into()))?
                .unwrap_or(0..meta.size);

            let reader = self
                .client
                .read(&make_absolute_file(location))
                .await
                .to_object_store_err()?;
            let start: usize = range
                .start
                .try_into()
                .expect("unable to convert range.start to usize");
            let end: usize = range
                .end
                .try_into()
                .expect("unable to convert range.end to usize");
            let stream = reader
                .read_range_stream(start, end - start)
                .map(|b| b.to_object_store_err())
                .boxed();

            (range, GetResultPayload::Stream(stream))
        };

        Ok(GetResult {
            payload,
            meta,
            range,
            attributes: Default::default(),
        })
    }

    /// Delete a stream of objects.
    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        let client = self.client.clone();
        locations
            .map(move |location| {
                let client = client.clone();
                async move {
                    let location = location?;
                    client
                        .delete(&make_absolute_file(&location), false)
                        .await
                        .to_object_store_err()?;
                    Ok(location)
                }
            })
            .buffered(10)
            .boxed()
    }

    /// List all the objects with the given prefix.
    ///
    /// Prefixes are evaluated on a path segment basis, i.e. `foo/bar/` is a prefix of `foo/bar/x` but not of
    /// `foo/bar_baz/x`.
    ///
    /// Note: the order of returned [`ObjectMeta`] is not guaranteed
    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        let absolute_dir = prefix.map(make_absolute_file).unwrap_or("/".to_string());

        let status_stream = self
            .client
            .list_status_iter(&absolute_dir, true)
            .into_stream()
            .filter(move |res| {
                let result = match res {
                    // Directories aren't a thing in object stores so ignore them, and if a file is listed
                    // directly that should be ignored as well
                    Ok(status) => !status.isdir && status.path != absolute_dir,
                    // Listing by prefix should just return an empty list if the prefix isn't found
                    Err(HdfsError::FileNotFound(_)) => false,
                    _ => true,
                };
                future::ready(result)
            })
            .map(|res| res.map_or_else(|e| Err(e).to_object_store_err(), |s| get_object_meta(&s)));

        Box::pin(status_stream)
    }

    /// List objects with the given prefix and an implementation specific
    /// delimiter. Returns common prefixes (directories) in addition to object
    /// metadata.
    ///
    /// Prefixes are evaluated on a path segment basis, i.e. `foo/bar/` is a prefix of `foo/bar/x` but not of
    /// `foo/bar_baz/x`.
    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        let absolute_dir = prefix.map(make_absolute_file).unwrap_or("/".to_string());

        let mut status_stream = self
            .client
            .list_status_iter(&absolute_dir, false)
            .into_stream()
            .filter(move |res| {
                let result = match res {
                    // If a file is listed directly it should be ignored
                    Ok(status) => status.path != absolute_dir,
                    // Listing by prefix should just return an empty list if the prefix isn't found
                    Err(HdfsError::FileNotFound(_)) => false,
                    _ => true,
                };
                future::ready(result)
            });

        let mut statuses = Vec::<FileStatus>::new();
        while let Some(status) = status_stream.next().await {
            statuses.push(status.to_object_store_err()?);
        }

        let mut dirs: Vec<Path> = Vec::new();
        for status in statuses.iter().filter(|s| s.isdir) {
            dirs.push(Path::parse(&status.path)?)
        }

        let mut files: Vec<ObjectMeta> = Vec::new();
        for status in statuses.iter().filter(|s| !s.isdir) {
            files.push(get_object_meta(status)?)
        }

        Ok(ListResult {
            common_prefixes: dirs,
            objects: files,
        })
    }

    async fn rename_opts(&self, from: &Path, to: &Path, options: RenameOptions) -> Result<()> {
        // Make sure the parent directory exists
        let mut parent: Vec<_> = to.parts().collect();
        parent.pop();

        if !parent.is_empty() {
            let parent_path: Path = parent.into_iter().collect();
            self.client
                .mkdirs(&make_absolute_dir(&parent_path), 0o755, true)
                .await
                .to_object_store_err()?;
        }

        let overwrite = match options.target_mode {
            RenameTargetMode::Overwrite => true,
            RenameTargetMode::Create => false,
        };

        Ok(self
            .client
            .rename(
                &make_absolute_file(from),
                &make_absolute_file(to),
                overwrite,
            )
            .await
            .to_object_store_err()?)
    }

    /// Copy an object from one path to another with options.
    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        let overwrite = match options.mode {
            CopyMode::Create => {
                // Eagerly check if the target file exists first before wasting time copying
                match self.client.get_file_info(&make_absolute_file(to)).await {
                    Ok(_) => {
                        return Err(HdfsError::AlreadyExists(make_absolute_file(to)))
                            .to_object_store_err();
                    }
                    Err(HdfsError::FileNotFound(_)) => false,
                    Err(e) => return Err(e).to_object_store_err(),
                }
            }
            CopyMode::Overwrite => true,
        };

        let write_options = WriteOptions {
            overwrite,
            ..Default::default()
        };

        let file = self
            .client
            .read(&make_absolute_file(from))
            .await
            .to_object_store_err()?;
        let mut stream = file.read_range_stream(0, file.file_length()).boxed();

        let mut new_file = self
            .client
            .create(&make_absolute_file(to), write_options)
            .await
            .to_object_store_err()?;

        while let Some(bytes) = stream.next().await.transpose().to_object_store_err()? {
            new_file.write(bytes).await.to_object_store_err()?;
        }
        new_file.close().await.to_object_store_err()?;

        Ok(())
    }
}

trait HdfsErrorConvert<T> {
    fn to_object_store_err(self) -> Result<T>;
}

impl<T> HdfsErrorConvert<T> for hdfs_native::Result<T> {
    fn to_object_store_err(self) -> Result<T> {
        self.map_err(|err| match err {
            HdfsError::FileNotFound(path) => object_store::Error::NotFound {
                path: path.clone(),
                source: Box::new(HdfsError::FileNotFound(path)),
            },
            HdfsError::AlreadyExists(path) => object_store::Error::AlreadyExists {
                path: path.clone(),
                source: Box::new(HdfsError::AlreadyExists(path)),
            },
            _ => object_store::Error::Generic {
                store: "HdfsObjectStore",
                source: Box::new(err),
            },
        })
    }
}

type PartSender = mpsc::UnboundedSender<(oneshot::Sender<Result<()>>, PutPayload)>;

// Create a fake multipart writer the creates an uploader to a temp file as a background
// task, and submits new parts to be uploaded to a queue for this task.
// A once cell is used to track whether a part has finished writing or not.
// On completing, rename the file to the actual target.
struct HdfsMultipartWriter {
    client: Client,
    sender: Option<(JoinHandle<Result<()>>, PartSender)>,
    tmp_filename: String,
    final_filename: String,
}

impl HdfsMultipartWriter {
    fn new(client: Client, writer: FileWriter, tmp_filename: &str, final_filename: &str) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();

        Self {
            client,
            sender: Some((Self::start_writer_task(writer, receiver), sender)),
            tmp_filename: tmp_filename.to_string(),
            final_filename: final_filename.to_string(),
        }
    }

    fn start_writer_task(
        mut writer: FileWriter,
        mut part_receiver: mpsc::UnboundedReceiver<(oneshot::Sender<Result<()>>, PutPayload)>,
    ) -> JoinHandle<Result<()>> {
        task::spawn(async move {
            loop {
                match part_receiver.recv().await {
                    Some((sender, part)) => {
                        for bytes in part {
                            if let Err(e) = writer.write(bytes).await {
                                let _ = sender.send(Err(e).to_object_store_err());
                                return Err(generic_error("Failed to write all parts".into()));
                            }
                        }
                        let _ = sender.send(Ok(()));
                    }
                    None => return writer.close().await.to_object_store_err(),
                }
            }
        })
    }
}

impl std::fmt::Debug for HdfsMultipartWriter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HdfsMultipartWriter")
            .field("tmp_filename", &self.tmp_filename)
            .field("final_filename", &self.final_filename)
            .finish()
    }
}

#[async_trait]
impl MultipartUpload for HdfsMultipartWriter {
    fn put_part(&mut self, payload: PutPayload) -> UploadPart {
        let (result_sender, result_receiver) = oneshot::channel();

        if let Some((_, payload_sender)) = self.sender.as_ref() {
            if let Err(mpsc::error::SendError((result_sender, _))) =
                payload_sender.send((result_sender, payload))
            {
                let _ = result_sender.send(Err(generic_error("Write task failed".into())));
            }
        } else {
            let _ = result_sender.send(Err(generic_error(
                "Cannot put part after completing or aborting".into(),
            )));
        }

        async {
            result_receiver
                .await
                .unwrap_or_else(|_| Err(generic_error("Write task failed".into())))
        }
        .boxed()
    }

    async fn complete(&mut self) -> Result<PutResult> {
        // Drop the sender so the task knows no more data is coming
        if let Some((handle, sender)) = self.sender.take() {
            drop(sender);

            // Wait for the writer task to finish
            handle.await??;

            self.client
                .rename(&self.tmp_filename, &self.final_filename, true)
                .await
                .to_object_store_err()?;

            Ok(PutResult {
                e_tag: None,
                version: None,
            })
        } else {
            Err(generic_error(
                "Cannot call abort or complete multiple times".into(),
            ))
        }
    }

    async fn abort(&mut self) -> Result<()> {
        // Drop the sender so the task knows no more data is coming
        if let Some((handle, sender)) = self.sender.take() {
            drop(sender);

            // Wait for the writer task to finish
            handle.abort();

            self.client
                .delete(&self.tmp_filename, false)
                .await
                .to_object_store_err()?;

            Ok(())
        } else {
            Err(generic_error(
                "Cannot call abort or complete multiple times".into(),
            ))
        }
    }
}

/// ObjectStore paths always remove the leading slash, so add it back
fn make_absolute_file(path: &Path) -> String {
    format!("/{}", path.as_ref())
}

fn make_absolute_dir(path: &Path) -> String {
    if path.parts().count() > 0 {
        format!("/{}/", path.as_ref())
    } else {
        "/".to_string()
    }
}

fn get_etag(status: &FileStatus) -> String {
    let size = status.length;
    let mtime = status.modification_time;

    // Use an ETag scheme based on that used by many popular HTTP servers
    // <https://httpd.apache.org/docs/2.2/mod/core.html#fileetag>
    // <https://stackoverflow.com/questions/47512043/how-etags-are-generated-and-configured>
    format!("{mtime:x}-{size:x}")
}

fn get_object_meta(status: &FileStatus) -> Result<ObjectMeta> {
    Ok(ObjectMeta {
        location: Path::parse(&status.path)?,
        last_modified: DateTime::<Utc>::from_timestamp_millis(status.modification_time as i64)
            .ok_or(generic_error(
                "Last modified timestamp out of bounds".into(),
            ))?,
        size: status.length as u64,
        e_tag: Some(get_etag(status)),
        version: None,
    })
}

#[cfg(test)]
#[cfg(feature = "integration-test")]
mod test {
    use std::collections::HashSet;

    use object_store::integration::*;
    use serial_test::serial;
    use tokio::runtime::Runtime;

    use crate::HdfsObjectStoreBuilder;

    #[tokio::test]
    #[serial]
    async fn hdfs_test() {
        let dfs = hdfs_native::minidfs::MiniDfs::with_features(&HashSet::from([
            hdfs_native::minidfs::DfsFeatures::HA,
        ]));

        let integration = HdfsObjectStoreBuilder::new()
            .with_url(&dfs.url)
            .build()
            .unwrap();

        put_get_delete_list(&integration).await;
        list_uses_directories_correctly(&integration).await;
        list_with_delimiter(&integration).await;
        rename_and_copy(&integration).await;
        copy_if_not_exists(&integration).await;
        multipart_race_condition(&integration, true).await;
        multipart_out_of_order(&integration).await;
        get_opts(&integration).await;
        put_opts(&integration, false).await;
    }

    #[test]
    #[serial]
    fn test_no_tokio() {
        let dfs = hdfs_native::minidfs::MiniDfs::with_features(&HashSet::from([
            hdfs_native::minidfs::DfsFeatures::HA,
        ]));

        let integration = HdfsObjectStoreBuilder::new()
            .with_url(&dfs.url)
            .build()
            .unwrap();

        futures::executor::block_on(get_opts(&integration));

        let rt = Runtime::new().unwrap();

        let integration = HdfsObjectStoreBuilder::new()
            .with_url(&dfs.url)
            .with_io_runtime(rt.handle().clone())
            .build()
            .unwrap();

        futures::executor::block_on(get_opts(&integration));
    }
}
