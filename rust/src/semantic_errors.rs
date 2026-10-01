//! Which [`SemanticErrorKind`] a failure on the semantic path is.
//!
//! The sidecar's errors are typed — [`SemanticSearchError`] and the enums inside it — and
//! it leaves turning them into states a user is shown to the host: "Mapping those onto
//! user-facing states (`ready`, `corrupt`, `incompatible`, `model_missing`) is the host
//! application's job" (`semantic/official_index.rs`). This module is that mapping, and the
//! only place it is made; the semantic API calls it where each failure arises, with the
//! message it has always written.
//!
//! # By type, never by message
//!
//! Every decision here matches a variant. A message is for a person, and the sidecar is
//! free to reword one; a mapping that read them would move a failure from one kind to
//! another on a repin, with nothing to say so. The matches on [`SemanticSearchError`],
//! [`EmbeddingError`], [`VectorStoreError`] and [`ArtifactError`] name every variant and
//! have no wildcard, so a repin that adds one fails to compile here until someone has
//! decided what it is.
//!
//! Where the type alone does not settle it, a fact does, and still never the text:
//!
//! * the call the error came from ([`SidecarCall`]). A `Config` error refuses the caller's
//!   configuration when `configure_semantic` opens the sidecar, and is an internal fault
//!   anywhere after that.
//! * the file system, for the two variants the sidecar uses for more than one state.
//!   [`ArtifactError::MetadataUnusable`] is a missing artifact when its `manifest.json`
//!   does not exist and a damaged one when it does, and
//!   [`EmbeddingError::OnnxRuntimeUnavailable`] is a missing runtime when there is no file
//!   where the sidecar looks for one and an unusable one when there is.
//!
//! And where neither can, because the sidecar names the same error for both sides of a
//! comparison, the ambiguity is removed before the sidecar is asked: [`check_local_model`]
//! settles the installation's own values first, so what opening an artifact refuses
//! afterwards is the artifact's, the model's or the runtime's.
//!
//! What still cannot be told apart gets the broad kind that is true of all of it, and
//! `Internal` for faults in the engine or its files.

use crate::api::search_engine::{SemanticError, SemanticErrorKind};
use crate::semantic_corpus::CorpusStampError;
use otzaria_semantic_search::distribution::package::MANIFEST_FILENAME;
use otzaria_semantic_search::errors::{
    ArtifactError, EmbeddingError, SemanticSearchError, VectorStoreError,
};
use otzaria_semantic_search::semantic::backend::{ensure_pooling_is_implemented_for, Pooling};
use otzaria_semantic_search::semantic::embedding::EmbeddingConfig;
use otzaria_semantic_search::semantic::model_package::{onnx_package_root, ModelFormat};
use otzaria_semantic_search::semantic::official_index::LocalModel;
use otzaria_semantic_search::semantic::recipe::{EmbeddingTextRecipe, TextNormalizationRecipe};
use otzaria_semantic_search::semantic::versioning::IdentityField;
use std::ffi::OsString;
use std::path::Path;

/// The variable the sidecar takes the ONNX Runtime library's path from when the application
/// passes none, and the file it looks for beside the graph when neither names one. Both are
/// private constants of the sidecar's ONNX backend, repeated here because telling a missing
/// runtime from an unusable one means looking where it looks; the rule is the one
/// `SemanticConfigInput::model_path` documents to the application.
const ONNX_RUNTIME_ENV: &str = "OTZARIA_ONNX_RUNTIME";
#[cfg(target_os = "macos")]
const ONNX_RUNTIME_FILE_NAME: Option<&str> = Some("libonnxruntime.dylib");
#[cfg(target_os = "linux")]
const ONNX_RUNTIME_FILE_NAME: Option<&str> = Some("libonnxruntime.so");
#[cfg(target_os = "windows")]
const ONNX_RUNTIME_FILE_NAME: Option<&str> = Some("onnxruntime.dll");
/// The ONNX backend is built for desktop targets only; elsewhere an ONNX model is
/// `BackendUnavailable`, and no runtime is ever looked for.
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const ONNX_RUNTIME_FILE_NAME: Option<&str> = None;

/// The call a sidecar error came from, which is part of what the error means.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SidecarCall<'a> {
    /// `SemanticEngine::open`, from `configure_semantic`. Everything it refuses is the
    /// configuration it was handed: it loads no model and reads no artifact.
    Configure,
    /// `OfficialSemanticIndex::open`, from `open_semantic_artifact`, once
    /// [`check_local_model`] has passed the installation's own values: what it refuses is
    /// then the artifact, the model or the runtime.
    OpenArtifact {
        artifact_dir: &'a Path,
        model_path: &'a Path,
        /// The ONNX Runtime the application passed, the first place the sidecar looks.
        onnx_runtime: Option<&'a Path>,
    },
    /// An operation on an open session: indexing, the index diff, removal, reset or search.
    Session {
        model_path: &'a Path,
        onnx_runtime: Option<&'a Path>,
    },
}

impl SidecarCall<'_> {
    fn model_path(&self) -> Option<&Path> {
        match self {
            Self::Configure => None,
            Self::OpenArtifact { model_path, .. } | Self::Session { model_path, .. } => {
                Some(model_path)
            }
        }
    }

    /// The runtime the call's session was handed, if it was handed one. Configuring loads
    /// nothing, so it has none to look for.
    fn onnx_runtime(&self) -> Option<&Path> {
        match self {
            Self::Configure => None,
            Self::OpenArtifact { onnx_runtime, .. } | Self::Session { onnx_runtime, .. } => {
                *onnx_runtime
            }
        }
    }
}

/// `error` as a [`SemanticError`] that reads `message`. The caller writes the message, as
/// it always has; this adds the kind and the field, and changes nothing a person reads.
pub(crate) fn sidecar_error(
    error: &SemanticSearchError,
    call: SidecarCall<'_>,
    message: String,
) -> SemanticError {
    let (kind, field) = classify(error, call);
    SemanticError {
        kind,
        message,
        field,
    }
}

/// `error`, from a search, as a [`SemanticError`]. Its message says the search failed, unless
/// the search was cancelled, which is not a failure: that reads as any cancelled search does,
/// whichever look noticed it.
pub(crate) fn search_error(error: &SemanticSearchError, call: SidecarCall<'_>) -> SemanticError {
    match classify(error, call) {
        (SemanticErrorKind::Cancelled, _) => SemanticError::cancelled(),
        (kind, field) => SemanticError {
            kind,
            message: format!("semantic search failed: {error}"),
            field,
        },
    }
}

/// A ranking `RankingProfile::validate` refused, before the search it came with ran. Checking
/// a ranking loads nothing and opens nothing, as configuring does not, so it is classified as
/// a configuration is; the variant is the same whichever call it came from.
pub(crate) fn ranking_error(error: &SemanticSearchError) -> SemanticError {
    sidecar_error(
        error,
        SidecarCall::Configure,
        format!("the ranking passed with this search is refused: {error}"),
    )
}

/// The name `SemanticRankingOptions` gives a ranking parameter that the sidecar names by its
/// path in `RankingProfile`. They are the same, field for field, but for RRF's `k`, which the
/// options carry as `rrf_k` beside the strategy, since a Dart enum carries no value.
fn ranking_field(parameter: &str) -> String {
    match parameter {
        "fusion_strategy.k" => "rrf_k",
        other => other,
    }
    .to_string()
}

/// A corpus stamp that cannot vouch for the open index, as a [`SemanticError`].
///
/// A stamp this build cannot read is no stamp to it, so it is `IndexNotStamped` as a
/// missing one is; an I/O failure reading it says nothing about the index, and is
/// `Internal`.
pub(crate) fn stamp_error(error: &CorpusStampError) -> SemanticError {
    let kind = match error {
        CorpusStampError::Missing { .. } | CorpusStampError::Unrecognized(_) => {
            SemanticErrorKind::IndexNotStamped
        }
        CorpusStampError::Outdated { .. } => SemanticErrorKind::IndexStampMismatch,
        CorpusStampError::Unreadable(_) => SemanticErrorKind::Internal,
    };
    SemanticError::new(kind, error.to_string())
}

/// The checks `OfficialSemanticIndex::open` makes of the installation's own half of the
/// identity before it reads the artifact, made first, by the same functions, in the same
/// order.
///
/// They have to come first because two of the sidecar's error types refuse either side.
/// `UnsupportedRecipeVersion` is the installation's text or normalization version when
/// these checks raise it, and the artifact's when its identity is verified; `LoadFailed` is
/// a token cap or width no model could be loaded with when the runtime's configuration
/// raises it, and a model that would not load when its backend does. Settled here, a
/// refusal of the installation's values is `InvalidInput`, and whatever the sidecar
/// refuses after them is the artifact's, the model's or the runtime's.
///
/// The error is the one the sidecar would have returned, converted the way its `?` converts
/// it, so `describe` writes the message the sidecar's refusal would have had.
pub(crate) fn check_local_model(
    model: &LocalModel,
    describe: impl FnOnce(&SemanticSearchError) -> String,
) -> Result<(), SemanticError> {
    let refused = |error: SemanticSearchError, field: Option<&str>| {
        let refusal = SemanticError::new(SemanticErrorKind::InvalidInput, describe(&error));
        match field {
            Some(field) => refusal.with_field(field),
            None => refusal,
        }
    };
    if let Err(error) = EmbeddingTextRecipe::from_version(model.embedding_text_version) {
        return Err(refused(error.into(), Some("embedding_text_version")));
    }
    if let Err(error) = TextNormalizationRecipe::from_version(model.normalization_version) {
        return Err(refused(error.into(), Some("normalization_version")));
    }
    // As `LocalModel::pooling_strategy` refuses it: a spelling that does not parse, or a
    // pooling no backend for the model's format performs, both as the caller's `Config`.
    let pooling = match Pooling::parse(&model.pooling).and_then(|pooling| {
        ensure_pooling_is_implemented_for(pooling, ModelFormat::of(&model.model_path))
            .map(|()| pooling)
    }) {
        Ok(pooling) => pooling,
        Err(error) => {
            return Err(refused(
                SemanticSearchError::Config(error.to_string()),
                Some("pooling"),
            ))
        }
    };
    // The runtime the sidecar builds, with the one batch size it embeds queries at. Its
    // refusals here are of the width or the token cap, and say which in their text only.
    let config = EmbeddingConfig {
        model_path: model.model_path.clone(),
        embedding_dim: model.embedding_dim,
        max_tokens: model.max_tokens,
        batch_size: 1,
        pooling,
    };
    match config.validate() {
        Ok(()) => Ok(()),
        Err(error) => Err(refused(error.into(), None)),
    }
}

/// The kind of `error`, and the field it is about when it names one.
fn classify(
    error: &SemanticSearchError,
    call: SidecarCall<'_>,
) -> (SemanticErrorKind, Option<String>) {
    use SemanticErrorKind as K;
    match error {
        SemanticSearchError::EmbeddingRuntime(error) => embedding_kind(error, call),
        SemanticSearchError::Artifact(error) => match call {
            SidecarCall::OpenArtifact { artifact_dir, .. } => artifact_kind(error, artifact_dir),
            // Opening a session built on this device reads no artifact. The one artifact
            // error it raises is a recipe version the configuration names and this build has
            // no code for.
            SidecarCall::Configure => match error {
                ArtifactError::UnsupportedRecipeVersion { field, .. } => {
                    (K::InvalidInput, Some((*field).to_string()))
                }
                _ => (K::Internal, None),
            },
            SidecarCall::Session { .. } => (K::Internal, None),
        },
        SemanticSearchError::VectorStore(error) => (store_kind(error, call), None),
        // The sidecar's validation of a configuration, when it is handed one. After that it
        // uses `Config` for its own inconsistencies: a runtime that reports no checksum, a
        // backend that returns too few vectors, a model not loaded yet.
        SemanticSearchError::Config(_) => match call {
            SidecarCall::Configure => (K::InvalidInput, None),
            SidecarCall::OpenArtifact { .. } | SidecarCall::Session { .. } => (K::Internal, None),
        },
        SemanticSearchError::IncompatibleIndex { .. } => (K::ReindexRequired, None),
        SemanticSearchError::ReadOnlyIndex { .. } => (K::ReadOnlySession, None),
        // A search the caller abandoned through its token, stopped at one of the sidecar's
        // looks at it. Not a failure, which `search_error` does not say it is.
        SemanticSearchError::Cancelled => (K::Cancelled, None),
        // A ranking passed with one search, refused before the search ran: the caller's
        // value, named as `SemanticRankingOptions` names it.
        SemanticSearchError::InvalidRankingParameter { parameter, .. } => {
            (K::InvalidInput, Some(ranking_field(parameter)))
        }
        SemanticSearchError::Manifest(_)
        | SemanticSearchError::Chunking(_)
        | SemanticSearchError::Fusion(_)
        | SemanticSearchError::Io(_)
        | SemanticSearchError::Serde(_) => (K::Internal, None),
    }
}

/// The kind of a vector store failure.
fn store_kind(error: &VectorStoreError, call: SidecarCall<'_>) -> SemanticErrorKind {
    use SemanticErrorKind as K;
    match error {
        // The read-only store loads an artifact's payload and checks every record as it
        // does, so at open a corrupted store is a damaged artifact.
        VectorStoreError::Corrupted { .. } if matches!(call, SidecarCall::OpenArtifact { .. }) => {
            K::ArtifactCorrupt
        }
        // A scan stopped by its token. The sidecar lifts it to `SemanticSearchError::
        // Cancelled` itself, so it arrives wrapped only if some layer skips that
        // conversion, and it is the same outcome either way.
        VectorStoreError::Cancelled => classify(&SemanticSearchError::Cancelled, call).0,
        // A session's in-memory store failing, and any other store failure, is a fault.
        VectorStoreError::NotInitialized { .. }
        | VectorStoreError::OpenFailed { .. }
        | VectorStoreError::InsertFailed { .. }
        | VectorStoreError::SearchFailed { .. }
        | VectorStoreError::DeleteFailed { .. }
        | VectorStoreError::CommitFailed { .. }
        | VectorStoreError::DimensionMismatch { .. }
        | VectorStoreError::Corrupted { .. } => K::Internal,
    }
}

/// The kind of a model failure. The model is the same whichever call loaded it, so this
/// does not depend on the call, except for where it looks for ONNX Runtime.
fn embedding_kind(
    error: &EmbeddingError,
    call: SidecarCall<'_>,
) -> (SemanticErrorKind, Option<String>) {
    use SemanticErrorKind as K;
    let field = |name: &str| Some(name.to_string());
    match error {
        EmbeddingError::ModelNotFound { .. } => (K::ModelMissing, None),
        EmbeddingError::TokenizerNotFound { .. } => (K::TokenizerMissing, None),
        // A file that is not a model of its format, and a model its backend could not load
        // (llama.cpp or ONNX Runtime refusing the weights, a session that will not
        // allocate): to the application both are a model to download again.
        EmbeddingError::InvalidModelFile { .. } | EmbeddingError::LoadFailed { .. } => {
            (K::ModelInvalid, None)
        }
        EmbeddingError::BackendUnavailable { .. } => (K::BackendNotInBuild, None),
        EmbeddingError::OnnxRuntimeUnavailable { .. } => (
            runtime_kind(
                call.onnx_runtime(),
                std::env::var_os(ONNX_RUNTIME_ENV),
                call.model_path(),
            ),
            None,
        ),
        // A configuration's pooling, refused by the sidecar's model-side checks.
        EmbeddingError::UnknownPooling { .. }
        | EmbeddingError::PoolingNotImplemented { .. }
        | EmbeddingError::PoolingNotForFormat { .. } => (K::InvalidInput, field("pooling")),
        // The model the backend loaded is not the one the identity or the configuration
        // describes: it pools, or measures, otherwise.
        EmbeddingError::PoolingMismatch { .. } => (K::ModelIdentityMismatch, field("pooling")),
        EmbeddingError::DimensionMismatch { .. } => {
            (K::ModelIdentityMismatch, field("embedding_dim"))
        }
        EmbeddingError::InferenceFailed { .. }
        | EmbeddingError::TokenizationUnsupported { .. }
        | EmbeddingError::NotLoaded => (K::Internal, None),
    }
}

/// `OnnxRuntimeUnavailable` covers every way the runtime could not be loaded, so whether
/// there is a file where the sidecar looks for one is what separates them: none is a
/// missing runtime, and one that did not load is unusable — not a library, not ONNX
/// Runtime, too old, refused earlier, or not the one this process already runs.
///
/// Where it looks is three places, and the first one that is set is the only one looked
/// at; a later place is never tried in its place (the sidecar's `resolve_runtime_path`,
/// private to its ONNX backend):
///
/// 1. `passed`, the path the application passed (`EmbeddingDeployment::onnx_runtime`);
/// 2. `env_value`, `OTZARIA_ONNX_RUNTIME`;
/// 3. the platform's file name beside the graph at `model_path`.
///
/// A passed path or a variable that is set but empty names nothing, which the sidecar
/// refuses as such, so it is missing here; nothing beside the graph is looked at then
/// either. Both are taken as arguments, as the sidecar takes them, so the rule is testable
/// without changing the process environment.
fn runtime_kind(
    passed: Option<&Path>,
    env_value: Option<OsString>,
    model_path: Option<&Path>,
) -> SemanticErrorKind {
    let present = match (passed, env_value) {
        (Some(passed), _) => !passed.as_os_str().is_empty() && passed.exists(),
        (None, Some(named)) => !named.is_empty() && Path::new(&named).exists(),
        (None, None) => match (model_path, ONNX_RUNTIME_FILE_NAME) {
            (Some(graph), Some(file)) => onnx_package_root(graph).join(file).exists(),
            _ => false,
        },
    };
    if present {
        SemanticErrorKind::OnnxRuntimeUnusable
    } else {
        SemanticErrorKind::OnnxRuntimeMissing
    }
}

/// The kind of an artifact the sidecar refused at open, and the field it names.
fn artifact_kind(
    error: &ArtifactError,
    artifact_dir: &Path,
) -> (SemanticErrorKind, Option<String>) {
    use SemanticErrorKind as K;
    match error {
        // Absent, unreadable or not JSON: the variant does not say which, and the file
        // system does. Without a `manifest.json` nothing is installed there; with one,
        // what is installed is damaged — a `payloads.json` missing beside it included.
        ArtifactError::MetadataUnusable { .. } => {
            if artifact_dir.join(MANIFEST_FILENAME).exists() {
                (K::ArtifactCorrupt, None)
            } else {
                (K::ArtifactMissing, None)
            }
        }
        // Metadata, a recipe or a field this build does not read: a sound artifact, built
        // by a newer build or for something else.
        ArtifactError::UnsupportedMetadataVersion { .. } => (
            K::ArtifactIncompatible,
            Some("metadata_version".to_string()),
        ),
        ArtifactError::UnsupportedRecipeVersion { field, .. }
        | ArtifactError::RecipeDisagreesWithIdentity { field, .. } => {
            (K::ArtifactIncompatible, Some(identity_path(field)))
        }
        // Every field that disagreed is in the message; `field` is the first, in the
        // sidecar's own order — corpus, then model, then store — which is also the order
        // in which installing the right thing fixes the rest.
        ArtifactError::IdentityMismatch { mismatches } => (
            K::ArtifactIncompatible,
            mismatches
                .first()
                .map(|mismatch| mismatch.field.path().to_string()),
        ),
        ArtifactError::UnexpectedArtifactDigest { .. } => (K::ArtifactNotPublished, None),
        // An identity field left blank or zero by whatever built it: the metadata does not
        // describe a usable artifact.
        ArtifactError::IncompleteIdentity { field, .. } => {
            (K::ArtifactCorrupt, Some(field.path().to_string()))
        }
        ArtifactError::NoPayload
        | ArtifactError::UnsafePayloadName { .. }
        | ArtifactError::MalformedPayloadChecksum { .. }
        | ArtifactError::PayloadMissing { .. }
        | ArtifactError::PayloadNotRegularFile { .. }
        | ArtifactError::PayloadChecksumFailed { .. }
        | ArtifactError::ManifestDisagreesWithPayload { .. } => (K::ArtifactCorrupt, None),
        // A path that cannot hold an artifact at all, such as `/` or `..`.
        ArtifactError::InvalidInstallTarget { .. } => {
            (K::InvalidInput, Some("artifact_dir".to_string()))
        }
        // An install interrupted and not resolvable, and any other I/O failure. Neither is
        // a damaged artifact, and the first must not be answered by downloading over it:
        // the only good copy may be parked beside the target.
        ArtifactError::InterruptedInstall { .. } | ArtifactError::Io { .. } => (K::Internal, None),
    }
}

/// A recipe field the sidecar names bare (`embedding_text_version`) by its path in the
/// artifact's identity (`model.embedding_text_version`), as an identity mismatch names it,
/// so `field` reads one way for every incompatible artifact. A name the identity does not
/// carry, `chunking_version`, stays as it is.
fn identity_path(field: &str) -> String {
    [
        IdentityField::EmbeddingTextVersion,
        IdentityField::NormalizationVersion,
    ]
    .into_iter()
    .map(IdentityField::path)
    .find(|path| path.strip_prefix("model.") == Some(field))
    .unwrap_or(field)
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use otzaria_semantic_search::errors::ManifestError;
    use otzaria_semantic_search::semantic::versioning::IdentityMismatch;
    use std::path::PathBuf;
    use tempfile::TempDir;

    const MODEL: &str = "/models/model.onnx";

    fn session() -> SidecarCall<'static> {
        SidecarCall::Session {
            model_path: Path::new(MODEL),
            onnx_runtime: None,
        }
    }

    fn kind(error: SemanticSearchError, call: SidecarCall<'_>) -> SemanticErrorKind {
        classify(&error, call).0
    }

    /// Every model failure, by variant. The model is the same whichever call loaded it.
    #[test]
    fn each_model_failure_has_its_kind() {
        use SemanticErrorKind as K;
        let reason = || "reason".to_string();
        let cases: Vec<(EmbeddingError, SemanticErrorKind, Option<&str>)> = vec![
            (
                EmbeddingError::ModelNotFound { path: MODEL.into() },
                K::ModelMissing,
                None,
            ),
            (
                EmbeddingError::TokenizerNotFound { path: MODEL.into() },
                K::TokenizerMissing,
                None,
            ),
            (
                EmbeddingError::InvalidModelFile {
                    path: MODEL.into(),
                    reason: reason(),
                },
                K::ModelInvalid,
                None,
            ),
            (
                EmbeddingError::LoadFailed { reason: reason() },
                K::ModelInvalid,
                None,
            ),
            (
                EmbeddingError::BackendUnavailable { reason: reason() },
                K::BackendNotInBuild,
                None,
            ),
            (
                EmbeddingError::UnknownPooling {
                    found: "last_token".into(),
                    supported: reason(),
                },
                K::InvalidInput,
                Some("pooling"),
            ),
            (
                EmbeddingError::PoolingNotImplemented {
                    pooling: "mean".into(),
                    implemented: reason(),
                },
                K::InvalidInput,
                Some("pooling"),
            ),
            (
                EmbeddingError::PoolingNotForFormat {
                    pooling: "in-graph".into(),
                    format: "GGUF".into(),
                    implemented: reason(),
                    implemented_elsewhere: reason(),
                },
                K::InvalidInput,
                Some("pooling"),
            ),
            (
                EmbeddingError::PoolingMismatch {
                    backend: "b".into(),
                    configured: "last-token".into(),
                    actual: "in-graph".into(),
                },
                K::ModelIdentityMismatch,
                Some("pooling"),
            ),
            (
                EmbeddingError::DimensionMismatch {
                    expected: 256,
                    actual: 1024,
                },
                K::ModelIdentityMismatch,
                Some("embedding_dim"),
            ),
            (
                EmbeddingError::InferenceFailed { reason: reason() },
                K::Internal,
                None,
            ),
            (
                EmbeddingError::TokenizationUnsupported {
                    backend: "b".into(),
                    reason: reason(),
                },
                K::Internal,
                None,
            ),
            (EmbeddingError::NotLoaded, K::Internal, None),
        ];
        for (error, expected, field) in cases {
            let described = format!("{error:?}");
            for call in [
                session(),
                SidecarCall::OpenArtifact {
                    artifact_dir: Path::new("/artifact"),
                    model_path: Path::new(MODEL),
                    onnx_runtime: None,
                },
            ] {
                let (kind, named) =
                    classify(&SemanticSearchError::EmbeddingRuntime(clone(&error)), call);
                assert_eq!(kind, expected, "{described}");
                assert_eq!(named.as_deref(), field, "{described}");
            }
        }
    }

    /// `EmbeddingError` is not `Clone`; the table above needs each one twice.
    fn clone(error: &EmbeddingError) -> EmbeddingError {
        match error {
            EmbeddingError::ModelNotFound { path } => {
                EmbeddingError::ModelNotFound { path: path.clone() }
            }
            EmbeddingError::TokenizerNotFound { path } => {
                EmbeddingError::TokenizerNotFound { path: path.clone() }
            }
            EmbeddingError::LoadFailed { reason } => EmbeddingError::LoadFailed {
                reason: reason.clone(),
            },
            EmbeddingError::InvalidModelFile { path, reason } => EmbeddingError::InvalidModelFile {
                path: path.clone(),
                reason: reason.clone(),
            },
            EmbeddingError::BackendUnavailable { reason } => EmbeddingError::BackendUnavailable {
                reason: reason.clone(),
            },
            EmbeddingError::OnnxRuntimeUnavailable { reason } => {
                EmbeddingError::OnnxRuntimeUnavailable {
                    reason: reason.clone(),
                }
            }
            EmbeddingError::InferenceFailed { reason } => EmbeddingError::InferenceFailed {
                reason: reason.clone(),
            },
            EmbeddingError::UnknownPooling { found, supported } => EmbeddingError::UnknownPooling {
                found: found.clone(),
                supported: supported.clone(),
            },
            EmbeddingError::PoolingNotImplemented {
                pooling,
                implemented,
            } => EmbeddingError::PoolingNotImplemented {
                pooling: pooling.clone(),
                implemented: implemented.clone(),
            },
            EmbeddingError::PoolingNotForFormat {
                pooling,
                format,
                implemented,
                implemented_elsewhere,
            } => EmbeddingError::PoolingNotForFormat {
                pooling: pooling.clone(),
                format: format.clone(),
                implemented: implemented.clone(),
                implemented_elsewhere: implemented_elsewhere.clone(),
            },
            EmbeddingError::PoolingMismatch {
                backend,
                configured,
                actual,
            } => EmbeddingError::PoolingMismatch {
                backend: backend.clone(),
                configured: configured.clone(),
                actual: actual.clone(),
            },
            EmbeddingError::TokenizationUnsupported { backend, reason } => {
                EmbeddingError::TokenizationUnsupported {
                    backend: backend.clone(),
                    reason: reason.clone(),
                }
            }
            EmbeddingError::DimensionMismatch { expected, actual } => {
                EmbeddingError::DimensionMismatch {
                    expected: *expected,
                    actual: *actual,
                }
            }
            EmbeddingError::NotLoaded => EmbeddingError::NotLoaded,
        }
    }

    /// Missing or unusable is a file where the sidecar looks, and nothing else: the path the
    /// application passed when it passed one, else the variable when it is set, else the
    /// platform's file beside the graph.
    #[test]
    fn an_unloadable_runtime_is_missing_without_a_file_and_unusable_with_one() {
        use SemanticErrorKind as K;
        let dir = TempDir::new().unwrap();
        let graph = dir.path().join("model.onnx");
        let named = dir.path().join("named-runtime");
        std::fs::write(&named, b"not a library").unwrap();
        let absent = dir.path().join("absent");
        let variable = |path: &Path| Some(path.as_os_str().to_os_string());

        assert_eq!(
            runtime_kind(None, None, Some(&graph)),
            K::OnnxRuntimeMissing
        );
        assert_eq!(runtime_kind(None, None, None), K::OnnxRuntimeMissing);
        assert_eq!(
            runtime_kind(None, variable(&named), Some(&graph)),
            K::OnnxRuntimeUnusable
        );
        assert_eq!(
            runtime_kind(None, variable(&absent), None),
            K::OnnxRuntimeMissing
        );
        // Set but empty names nothing, and is not "unset": nothing beside the graph is
        // looked at either.
        assert_eq!(
            runtime_kind(None, Some(OsString::new()), Some(&graph)),
            K::OnnxRuntimeMissing
        );

        // A passed path is the first place, and once passed the only one: the variable
        // naming a file does not make a passed path that names none unusable, nor the
        // variable naming none make a passed file missing. Empty, it names nothing.
        assert_eq!(
            runtime_kind(Some(&named), None, Some(&graph)),
            K::OnnxRuntimeUnusable
        );
        assert_eq!(
            runtime_kind(Some(&named), variable(&absent), Some(&graph)),
            K::OnnxRuntimeUnusable
        );
        assert_eq!(
            runtime_kind(Some(&absent), variable(&named), Some(&graph)),
            K::OnnxRuntimeMissing
        );
        assert_eq!(
            runtime_kind(Some(Path::new("")), variable(&named), Some(&graph)),
            K::OnnxRuntimeMissing
        );

        if let Some(file) = ONNX_RUNTIME_FILE_NAME {
            std::fs::write(dir.path().join(file), b"not a library").unwrap();
            assert_eq!(
                runtime_kind(None, None, Some(&graph)),
                K::OnnxRuntimeUnusable
            );
            // The variable, when set, is the only place looked; and so is a passed path,
            // with or without the variable.
            assert_eq!(
                runtime_kind(None, variable(&absent), Some(&graph)),
                K::OnnxRuntimeMissing
            );
            assert_eq!(
                runtime_kind(Some(&absent), None, Some(&graph)),
                K::OnnxRuntimeMissing
            );
            assert_eq!(
                runtime_kind(Some(Path::new("")), None, Some(&graph)),
                K::OnnxRuntimeMissing
            );
        }
    }

    /// The runtime a session was handed is the first place a failure to load one is judged
    /// by, whichever call loaded the model, and the variable and the folder beside the
    /// graph are not looked at once it is there.
    #[test]
    fn the_runtime_a_session_was_handed_decides_whether_it_is_missing() {
        use SemanticErrorKind as K;
        let dir = TempDir::new().unwrap();
        let passed = dir.path().join("bundled-runtime");
        std::fs::write(&passed, b"not a library").unwrap();
        let absent = dir.path().join("absent");
        let unavailable = || {
            SemanticSearchError::EmbeddingRuntime(EmbeddingError::OnnxRuntimeUnavailable {
                reason: "r".into(),
            })
        };
        for (runtime, expected) in [
            (passed.as_path(), K::OnnxRuntimeUnusable),
            (absent.as_path(), K::OnnxRuntimeMissing),
        ] {
            for call in [
                SidecarCall::Session {
                    model_path: Path::new(MODEL),
                    onnx_runtime: Some(runtime),
                },
                SidecarCall::OpenArtifact {
                    artifact_dir: Path::new("/artifact"),
                    model_path: Path::new(MODEL),
                    onnx_runtime: Some(runtime),
                },
            ] {
                assert_eq!(kind(unavailable(), call), expected, "{call:?}");
            }
        }
    }

    fn mismatch(field: IdentityField) -> IdentityMismatch {
        IdentityMismatch {
            field,
            artifact: "a".into(),
            expected: "b".into(),
        }
    }

    /// Every artifact refusal, by variant; and a missing `manifest.json` is what makes
    /// unusable metadata a missing artifact rather than a damaged one.
    #[test]
    fn each_artifact_refusal_has_its_kind_and_field() {
        use SemanticErrorKind as K;
        let empty = TempDir::new().unwrap();
        let installed = TempDir::new().unwrap();
        std::fs::write(installed.path().join(MANIFEST_FILENAME), b"{").unwrap();
        let absent = empty.path().join("absent");
        let payload = || "vectors.bin".to_string();
        let unusable = || ArtifactError::MetadataUnusable {
            path: "manifest.json".into(),
            reason: "r".into(),
        };

        let cases: Vec<(ArtifactError, &Path, SemanticErrorKind, Option<&str>)> = vec![
            (unusable(), empty.path(), K::ArtifactMissing, None),
            (unusable(), &absent, K::ArtifactMissing, None),
            (unusable(), installed.path(), K::ArtifactCorrupt, None),
            (
                ArtifactError::UnsupportedMetadataVersion {
                    found: 9,
                    supported: 1,
                },
                installed.path(),
                K::ArtifactIncompatible,
                Some("metadata_version"),
            ),
            (
                ArtifactError::UnsupportedRecipeVersion {
                    field: "embedding_text_version",
                    found: 9,
                    supported: "1, 2".into(),
                },
                installed.path(),
                K::ArtifactIncompatible,
                Some("model.embedding_text_version"),
            ),
            (
                ArtifactError::UnsupportedRecipeVersion {
                    field: "normalization_version",
                    found: 9,
                    supported: "1".into(),
                },
                installed.path(),
                K::ArtifactIncompatible,
                Some("model.normalization_version"),
            ),
            (
                ArtifactError::RecipeDisagreesWithIdentity {
                    field: "chunking_version",
                    configured: 1,
                    declared: 2,
                },
                installed.path(),
                K::ArtifactIncompatible,
                Some("chunking_version"),
            ),
            (
                ArtifactError::IdentityMismatch {
                    mismatches: vec![
                        mismatch(IdentityField::LibraryVersion),
                        mismatch(IdentityField::ModelId),
                    ],
                },
                installed.path(),
                K::ArtifactIncompatible,
                Some("corpus.library_version"),
            ),
            (
                ArtifactError::IdentityMismatch {
                    mismatches: vec![mismatch(IdentityField::StoreFormatVersion)],
                },
                installed.path(),
                K::ArtifactIncompatible,
                Some("store.store_format_version"),
            ),
            (
                ArtifactError::UnexpectedArtifactDigest {
                    expected: "0".into(),
                    actual: "1".into(),
                },
                installed.path(),
                K::ArtifactNotPublished,
                None,
            ),
            (
                ArtifactError::IncompleteIdentity {
                    field: IdentityField::CorpusId,
                    reason: "is blank".into(),
                },
                installed.path(),
                K::ArtifactCorrupt,
                Some("corpus.corpus_id"),
            ),
            (
                ArtifactError::NoPayload,
                installed.path(),
                K::ArtifactCorrupt,
                None,
            ),
            (
                ArtifactError::UnsafePayloadName {
                    name: "../x".into(),
                    reason: "r".into(),
                },
                installed.path(),
                K::ArtifactCorrupt,
                None,
            ),
            (
                ArtifactError::MalformedPayloadChecksum { payload: payload() },
                installed.path(),
                K::ArtifactCorrupt,
                None,
            ),
            (
                ArtifactError::PayloadMissing { payload: payload() },
                installed.path(),
                K::ArtifactCorrupt,
                None,
            ),
            (
                ArtifactError::PayloadNotRegularFile { payload: payload() },
                installed.path(),
                K::ArtifactCorrupt,
                None,
            ),
            (
                ArtifactError::PayloadChecksumFailed {
                    payload: payload(),
                    expected: "0".into(),
                    actual: "1".into(),
                },
                installed.path(),
                K::ArtifactCorrupt,
                None,
            ),
            (
                ArtifactError::ManifestDisagreesWithPayload { reason: "r".into() },
                installed.path(),
                K::ArtifactCorrupt,
                None,
            ),
            (
                ArtifactError::InvalidInstallTarget { reason: "r".into() },
                installed.path(),
                K::InvalidInput,
                Some("artifact_dir"),
            ),
            (
                ArtifactError::InterruptedInstall { reason: "r".into() },
                installed.path(),
                K::Internal,
                None,
            ),
            (
                ArtifactError::Io {
                    context: "c".into(),
                    source: std::io::Error::other("e"),
                },
                installed.path(),
                K::Internal,
                None,
            ),
        ];
        for (error, artifact_dir, expected, field) in cases {
            let described = format!("{error:?} in {}", artifact_dir.display());
            let (kind, named) = classify(
                &SemanticSearchError::Artifact(error),
                SidecarCall::OpenArtifact {
                    artifact_dir,
                    model_path: Path::new(MODEL),
                    onnx_runtime: None,
                },
            );
            assert_eq!(kind, expected, "{described}");
            assert_eq!(named.as_deref(), field, "{described}");
        }
    }

    /// The variants whose meaning depends on the call they came from.
    #[test]
    fn a_configuration_refusal_is_invalid_input_and_a_later_one_is_internal() {
        use SemanticErrorKind as K;
        let artifact_dir = PathBuf::from("/artifact");
        let open = SidecarCall::OpenArtifact {
            artifact_dir: &artifact_dir,
            model_path: Path::new(MODEL),
            onnx_runtime: None,
        };
        let config =
            || SemanticSearchError::Config("embedding_dim must be greater than zero".into());
        assert_eq!(kind(config(), SidecarCall::Configure), K::InvalidInput);
        assert_eq!(kind(config(), open), K::Internal);
        assert_eq!(kind(config(), session()), K::Internal);

        let recipe = || {
            SemanticSearchError::Artifact(ArtifactError::UnsupportedRecipeVersion {
                field: "embedding_text_version",
                found: 99,
                supported: "1, 2".into(),
            })
        };
        assert_eq!(
            classify(&recipe(), SidecarCall::Configure),
            (K::InvalidInput, Some("embedding_text_version".to_string()))
        );
        assert_eq!(kind(recipe(), open), K::ArtifactIncompatible);
        assert_eq!(kind(recipe(), session()), K::Internal);

        let corrupted = || {
            SemanticSearchError::VectorStore(VectorStoreError::Corrupted {
                reason: "vector checksum mismatch".into(),
            })
        };
        assert_eq!(kind(corrupted(), open), K::ArtifactCorrupt);
        assert_eq!(kind(corrupted(), session()), K::Internal);
        assert_eq!(
            kind(
                SemanticSearchError::VectorStore(VectorStoreError::OpenFailed {
                    reason: "r".into()
                }),
                open
            ),
            K::Internal
        );
    }

    /// Every store failure, by variant: corruption is a damaged artifact only where an
    /// artifact's payload is being read, a scan stopped by its token is the same outcome
    /// however it arrives, and everything else is a fault.
    #[test]
    fn each_store_failure_has_its_kind() {
        use SemanticErrorKind as K;
        let open = SidecarCall::OpenArtifact {
            artifact_dir: Path::new("/artifact"),
            model_path: Path::new(MODEL),
            onnx_runtime: None,
        };
        let store = SemanticSearchError::VectorStore;
        let reason = || "r".to_string();
        let faults = || {
            [
                VectorStoreError::NotInitialized { path: reason() },
                VectorStoreError::OpenFailed { reason: reason() },
                VectorStoreError::InsertFailed { reason: reason() },
                VectorStoreError::SearchFailed { reason: reason() },
                VectorStoreError::DeleteFailed { reason: reason() },
                VectorStoreError::CommitFailed { reason: reason() },
                VectorStoreError::DimensionMismatch {
                    store_dim: 256,
                    vector_dim: 1024,
                },
            ]
        };
        for call in [open, session()] {
            for fault in faults() {
                let described = format!("{fault:?} in {call:?}");
                assert_eq!(kind(store(fault), call), K::Internal, "{described}");
            }
            assert_eq!(
                kind(store(VectorStoreError::Cancelled), call),
                kind(SemanticSearchError::Cancelled, call),
                "{call:?}"
            );
        }
        let corrupted = || store(VectorStoreError::Corrupted { reason: reason() });
        assert_eq!(kind(corrupted(), open), K::ArtifactCorrupt);
        assert_eq!(kind(corrupted(), session()), K::Internal);
    }

    /// The session-wide states the sidecar types, and the rest of its top-level variants.
    #[test]
    fn the_session_states_and_the_internal_faults_have_their_kinds() {
        use SemanticErrorKind as K;
        assert_eq!(
            kind(
                SemanticSearchError::IncompatibleIndex {
                    details: "model_id".into()
                },
                session()
            ),
            K::ReindexRequired
        );
        assert_eq!(
            kind(
                SemanticSearchError::ReadOnlyIndex {
                    operation: "index_books"
                },
                session()
            ),
            K::ReadOnlySession
        );
        // A ranking parameter is the caller's value, and is named as the options name it:
        // as the sidecar does, but for RRF's `k`.
        for (parameter, field) in [
            ("alpha_by_query_type.short", "alpha_by_query_type.short"),
            ("fusion_strategy.k", "rrf_k"),
        ] {
            let refused = SemanticSearchError::InvalidRankingParameter {
                parameter,
                value: "-0.2".into(),
                requirement: "a number from 0 to 1",
            };
            assert_eq!(
                classify(&refused, session()),
                (K::InvalidInput, Some(field.to_string()))
            );
            assert_eq!(ranking_error(&refused).field.as_deref(), Some(field));
        }
        assert_eq!(
            kind(SemanticSearchError::Cancelled, session()),
            K::Cancelled
        );
        for internal in [
            SemanticSearchError::Manifest(ManifestError::WriteFailed { reason: "r".into() }),
            SemanticSearchError::Fusion("f".into()),
            SemanticSearchError::Io(std::io::Error::other("e")),
            SemanticSearchError::Serde(serde_json::from_str::<u32>("x").unwrap_err()),
        ] {
            let described = format!("{internal:?}");
            assert_eq!(kind(internal, session()), K::Internal, "{described}");
        }
    }

    /// A search's own failure says it failed; a cancelled one does not, and reads as a
    /// cancel this crate noticed itself does, however the sidecar delivered it.
    #[test]
    fn a_cancelled_search_is_not_reported_as_a_failed_one() {
        use SemanticErrorKind as K;
        for cancelled in [
            SemanticSearchError::Cancelled,
            SemanticSearchError::VectorStore(VectorStoreError::Cancelled),
        ] {
            assert_eq!(
                search_error(&cancelled, session()),
                SemanticError::cancelled()
            );
        }
        let failed = search_error(&SemanticSearchError::Fusion("f".into()), session());
        assert_eq!(failed.kind, K::Internal);
        assert_eq!(failed.message, "semantic search failed: Fusion error: f");
    }

    /// The installation's own values are refused as `InvalidInput`, with the message the
    /// sidecar's own refusal of them has.
    #[test]
    fn the_installations_own_values_are_invalid_input_with_the_sidecars_message() {
        let valid = LocalModel {
            model_path: PathBuf::from(MODEL),
            model_id: "m".into(),
            model_quantization: "int8".into(),
            embedding_dim: 256,
            pooling: "in-graph".into(),
            max_tokens: 256,
            embedding_text_version: 2,
            normalization_version: 1,
            chunking_identity: 1,
        };
        check_local_model(&valid, |_| unreachable!("a valid identity")).unwrap();

        type Spoil = fn(&mut LocalModel);
        let spoiled: [(Option<&str>, &str, Spoil); 6] = [
            (
                Some("embedding_text_version"),
                "embedding_text_version",
                |m| m.embedding_text_version = 99,
            ),
            (
                Some("normalization_version"),
                "normalization_version",
                |m| m.normalization_version = 99,
            ),
            (Some("pooling"), "last_token", |m| {
                m.pooling = "last_token".into()
            }),
            // A pooling this format's backend does not perform.
            (Some("pooling"), "last-token", |m| {
                m.pooling = "last-token".into()
            }),
            (None, "max_tokens is 1", |m| m.max_tokens = 1),
            (None, "embedding_dim is 0", |m| m.embedding_dim = 0),
        ];
        for (field, named, spoil) in spoiled {
            let mut model = valid.clone();
            spoil(&mut model);
            let error =
                check_local_model(&model, |error| format!("refused: {error}")).expect_err(named);
            assert_eq!(error.kind, SemanticErrorKind::InvalidInput, "{named}");
            assert_eq!(error.field.as_deref(), field, "{named}");
            assert!(
                error.message.starts_with("refused: ") && error.message.contains(named),
                "{named}: {}",
                error.message
            );
        }
    }

    /// The stamp's own failures, by variant, with their messages unchanged.
    #[test]
    fn each_stamp_failure_has_its_kind() {
        use SemanticErrorKind as K;
        let index_path = PathBuf::from("/index");
        for (error, expected) in [
            (
                CorpusStampError::Missing {
                    index_path: index_path.clone(),
                },
                K::IndexNotStamped,
            ),
            (
                CorpusStampError::Unrecognized(anyhow::anyhow!("not a corpus stamp")),
                K::IndexNotStamped,
            ),
            (
                CorpusStampError::Outdated {
                    index_path: index_path.clone(),
                    stamped: "a".into(),
                    current: "b".into(),
                },
                K::IndexStampMismatch,
            ),
            (
                CorpusStampError::Unreadable(anyhow::anyhow!("permission denied")),
                K::Internal,
            ),
        ] {
            let classified = stamp_error(&error);
            assert_eq!(classified.kind, expected, "{error:?}");
            assert_eq!(classified.message, error.to_string());
            assert_eq!(classified.field, None);
        }
    }
}
