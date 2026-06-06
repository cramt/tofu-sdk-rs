//! A deliberately side-effecting example provider for the TypeScript test
//! harness (`../../harness`).
//!
//! Where `example-aws` is backing-free and deterministic, this provider's whole
//! purpose is to leave an observable trace: each resource persists its
//! attributes to a JSON file under `${output_dir}` when created or updated, and
//! removes that file when destroyed. The harness applies a sequence of
//! configurations (sharing one state file) and asserts the resulting set of JSON
//! files after each step — so the create / in-place-update / replace / delete
//! lifecycle is checked against its *real* effects, not just Terraform state.
//!
//! Two resources: `fs_file` (a flat `content` map) and `fs_document` (which
//! exercises **nested blocks** — a single `meta` block and repeatable `section`
//! blocks via `#[facet(terraform::block)]`).
//!
//! The recorded `action` (`"created"` / `"updated"`) lives only in the written
//! file, never in the schema. That keeps it decoupled from Terraform's
//! computed-attribute consistency rules (an in-place update may not change a
//! computed value that planning left known) while still letting the harness see
//! which handler ran.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use facet::Facet;
use terraform_provider::terraform;
use terraform_runtime::{async_trait, serve, Provider, Resource, ResourceError};

/// Provider-level configuration: where resource JSON files are written.
#[derive(Facet)]
struct FsConfig {
    /// Absolute path to the directory resource files are written under. The
    /// harness points this at a fresh temp dir per configuration.
    #[facet(terraform::required)]
    output_dir: String,
}

/// The configured provider state shared by every handler — here, just the
/// resolved output directory.
struct FsClient {
    output_dir: PathBuf,
}

/// A dummy file-backed resource. `name` is the file's stem (and is `force_new`,
/// so a rename replaces the resource — old file deleted, new file created);
/// `content` is an arbitrary string map written verbatim; `id` is a computed
/// identifier derived from the name.
#[derive(Facet)]
#[facet(terraform::resource("fs_file"))]
struct FileModel {
    /// The file stem under `output_dir` (`<name>.json`). Renaming forces replace.
    #[facet(terraform::required)]
    #[facet(terraform::force_new)]
    name: String,

    /// Free-form contents recorded into the file. In-place updatable.
    content: Option<HashMap<String, String>>,

    /// A stable computed id derived from `name` (`file:<name>`).
    #[facet(terraform::computed)]
    id: String,
}

/// The on-disk record written for a resource. Not part of the schema — it is the
/// observable side effect the harness asserts on. `content` is flattened to a
/// (possibly empty) object so the file always has a stable shape.
#[derive(Facet)]
struct FileRecord {
    name: String,
    id: String,
    action: String,
    content: HashMap<String, String>,
}

/// The handler for `fs_file`, holding the configured output directory.
struct FileHandler {
    client: Arc<FsClient>,
}

impl FileHandler {
    /// The path `<output_dir>/<name>.json`.
    fn file_path(&self, name: &str) -> PathBuf {
        self.client.output_dir.join(format!("{name}.json"))
    }

    /// Compute the model's derived attributes and write its record to disk under
    /// `action`, returning the completed model.
    fn persist(&self, mut model: FileModel, action: &str) -> Result<FileModel, ResourceError> {
        model.id = format!("file:{}", model.name);

        let record = FileRecord {
            name: model.name.clone(),
            id: model.id.clone(),
            action: action.to_string(),
            content: model.content.clone().unwrap_or_default(),
        };
        let json = facet_json::to_string(&record).map_err(|e| {
            ResourceError::new("failed to encode record").with_detail(e.to_string())
        })?;

        fs::create_dir_all(&self.client.output_dir).map_err(|e| {
            ResourceError::new("failed to create output_dir").with_detail(e.to_string())
        })?;
        fs::write(self.file_path(&model.name), json).map_err(|e| {
            ResourceError::new("failed to write resource file").with_detail(e.to_string())
        })?;

        Ok(model)
    }
}

#[async_trait]
impl Resource for FileHandler {
    type Model = FileModel;

    async fn create(&self, planned: FileModel) -> Result<FileModel, ResourceError> {
        self.persist(planned, "created")
    }

    async fn update(
        &self,
        planned: FileModel,
        _prior: FileModel,
    ) -> Result<FileModel, ResourceError> {
        self.persist(planned, "updated")
    }

    async fn delete(&self, prior: FileModel) -> Result<(), ResourceError> {
        let path = self.file_path(&prior.name);
        // Tolerate an already-absent file so destroy stays idempotent.
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => {
                Err(ResourceError::new("failed to delete resource file").with_detail(e.to_string()))
            }
        }
    }
}

// --- fs_document: a resource exercising nested blocks ----------------------
//
// Demonstrates `#[facet(terraform::block)]`: `meta` is a single optional block
// and `section` is a repeatable list block. Both are written verbatim to the
// resource's JSON file, so the harness can assert that block config round-trips
// through the schema and codec.

/// A single optional `meta { … }` block.
#[derive(Facet, Clone)]
struct Meta {
    /// Who authored the document.
    #[facet(terraform::required)]
    author: String,
    /// An optional free-form note.
    note: Option<String>,
}

/// A repeatable `section { … }` block.
#[derive(Facet, Clone)]
struct Section {
    /// The section heading.
    #[facet(terraform::required)]
    heading: String,
    /// Optional section body text.
    body: Option<String>,
}

/// A document resource whose schema uses nested blocks.
#[derive(Facet)]
#[facet(terraform::resource("fs_document"))]
struct DocumentModel {
    /// The file stem under `output_dir` (`<name>.doc.json`). Renaming replaces.
    #[facet(terraform::required)]
    #[facet(terraform::force_new)]
    name: String,

    /// A single optional metadata block.
    #[facet(terraform::block)]
    meta: Option<Meta>,

    /// Zero or more ordered section blocks.
    #[facet(terraform::block)]
    section: Vec<Section>,

    /// A stable computed id derived from `name` (`doc:<name>`).
    #[facet(terraform::computed)]
    id: String,
}

/// The on-disk record for a document — the observable side effect.
#[derive(Facet)]
struct DocumentRecord {
    name: String,
    id: String,
    action: String,
    meta: Option<Meta>,
    sections: Vec<Section>,
}

/// The handler for `fs_document`.
struct DocumentHandler {
    client: Arc<FsClient>,
}

impl DocumentHandler {
    /// The path `<output_dir>/<name>.doc.json`.
    fn file_path(&self, name: &str) -> PathBuf {
        self.client.output_dir.join(format!("{name}.doc.json"))
    }

    fn persist(
        &self,
        mut model: DocumentModel,
        action: &str,
    ) -> Result<DocumentModel, ResourceError> {
        model.id = format!("doc:{}", model.name);

        let record = DocumentRecord {
            name: model.name.clone(),
            id: model.id.clone(),
            action: action.to_string(),
            meta: model.meta.clone(),
            sections: model.section.clone(),
        };
        let json = facet_json::to_string(&record).map_err(|e| {
            ResourceError::new("failed to encode record").with_detail(e.to_string())
        })?;

        fs::create_dir_all(&self.client.output_dir).map_err(|e| {
            ResourceError::new("failed to create output_dir").with_detail(e.to_string())
        })?;
        fs::write(self.file_path(&model.name), json).map_err(|e| {
            ResourceError::new("failed to write resource file").with_detail(e.to_string())
        })?;

        Ok(model)
    }
}

#[async_trait]
impl Resource for DocumentHandler {
    type Model = DocumentModel;

    async fn create(&self, planned: DocumentModel) -> Result<DocumentModel, ResourceError> {
        self.persist(planned, "created")
    }

    async fn update(
        &self,
        planned: DocumentModel,
        _prior: DocumentModel,
    ) -> Result<DocumentModel, ResourceError> {
        self.persist(planned, "updated")
    }

    async fn delete(&self, prior: DocumentModel) -> Result<(), ResourceError> {
        match fs::remove_file(self.file_path(&prior.name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => {
                Err(ResourceError::new("failed to delete resource file").with_detail(e.to_string()))
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let provider = Provider::builder()
        .provider_config::<FsConfig>()
        .configure(|cfg: FsConfig| async move {
            Arc::new(FsClient {
                output_dir: PathBuf::from(cfg.output_dir),
            })
        })
        .resource_with(|client: Arc<FsClient>| FileHandler { client })
        .resource_with(|client: Arc<FsClient>| DocumentHandler { client })
        .build()
        .expect("provider definition is valid");

    if let Err(err) = serve(provider).await {
        eprintln!("example-fs: failed to serve: {err}");
        std::process::exit(1);
    }
}
