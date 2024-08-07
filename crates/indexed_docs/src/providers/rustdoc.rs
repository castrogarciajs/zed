mod item;
mod to_markdown;

pub use item::*;
pub use to_markdown::convert_rustdoc_to_markdown;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use collections::{HashSet, VecDeque};
use fs::Fs;
use futures::AsyncReadExt;
use http::{AsyncBody, HttpClient, HttpClientWithUrl};

use crate::{IndexedDocsDatabase, IndexedDocsProvider, PackageName, ProviderId};

#[derive(Debug)]
struct RustdocItemWithHistory {
    pub item: RustdocItem,
    #[cfg(debug_assertions)]
    pub history: Vec<String>,
}

#[async_trait]
pub trait RustdocProvider {
    async fn fetch_page(
        &self,
        package: &PackageName,
        item: Option<&RustdocItem>,
    ) -> Result<Option<String>>;
}

pub struct RustdocIndexer {
    provider: Box<dyn RustdocProvider + Send + Sync + 'static>,
}

impl RustdocIndexer {
    pub fn new(provider: Box<dyn RustdocProvider + Send + Sync + 'static>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl IndexedDocsProvider for RustdocIndexer {
    fn id(&self) -> ProviderId {
        ProviderId::rustdoc()
    }

    fn database_path(&self) -> PathBuf {
        paths::support_dir().join("docs/rust/rustdoc-db.1.mdb")
    }

    async fn index(&self, package: PackageName, database: Arc<IndexedDocsDatabase>) -> Result<()> {
        let Some(package_root_content) = self.provider.fetch_page(&package, None).await? else {
            return Ok(());
        };

        let (crate_root_markdown, items) =
            convert_rustdoc_to_markdown(package_root_content.as_bytes())?;

        database
            .insert(package.to_string(), crate_root_markdown)
            .await?;

        let mut seen_items = HashSet::from_iter(items.clone());
        let mut items_to_visit: VecDeque<RustdocItemWithHistory> =
            VecDeque::from_iter(items.into_iter().map(|item| RustdocItemWithHistory {
                item,
                #[cfg(debug_assertions)]
                history: Vec::new(),
            }));

        while let Some(item_with_history) = items_to_visit.pop_front() {
            let item = &item_with_history.item;

            let Some(result) = self
                .provider
                .fetch_page(&package, Some(&item))
                .await
                .with_context(|| {
                    #[cfg(debug_assertions)]
                    {
                        format!(
                            "failed to fetch {item:?}: {history:?}",
                            history = item_with_history.history
                        )
                    }

                    #[cfg(not(debug_assertions))]
                    {
                        format!("failed to fetch {item:?}")
                    }
                })?
            else {
                continue;
            };

            let (markdown, referenced_items) = convert_rustdoc_to_markdown(result.as_bytes())?;

            database
                .insert(format!("{package}::{}", item.display()), markdown)
                .await?;

            let parent_item = item;
            for mut item in referenced_items {
                if seen_items.contains(&item) {
                    continue;
                }

                seen_items.insert(item.clone());

                item.path.extend(parent_item.path.clone());
                match parent_item.kind {
                    RustdocItemKind::Mod => {
                        item.path.push(parent_item.name.clone());
                    }
                    _ => {}
                }

                items_to_visit.push_back(RustdocItemWithHistory {
                    #[cfg(debug_assertions)]
                    history: {
                        let mut history = item_with_history.history.clone();
                        history.push(item.url_path());
                        history
                    },
                    item,
                });
            }
        }

        Ok(())
    }
}

pub struct LocalProvider {
    fs: Arc<dyn Fs>,
    cargo_workspace_root: PathBuf,
}

impl LocalProvider {
    pub fn new(fs: Arc<dyn Fs>, cargo_workspace_root: PathBuf) -> Self {
        Self {
            fs,
            cargo_workspace_root,
        }
    }
}

#[async_trait]
impl RustdocProvider for LocalProvider {
    async fn fetch_page(
        &self,
        crate_name: &PackageName,
        item: Option<&RustdocItem>,
    ) -> Result<Option<String>> {
        let mut local_cargo_doc_path = self.cargo_workspace_root.join("target/doc");
        local_cargo_doc_path.push(crate_name.as_ref());
        if let Some(item) = item {
            local_cargo_doc_path.push(item.url_path());
        } else {
            local_cargo_doc_path.push("index.html");
        }

        let Ok(contents) = self.fs.load(&local_cargo_doc_path).await else {
            return Ok(None);
        };

        Ok(Some(contents))
    }
}

pub struct DocsDotRsProvider {
    http_client: Arc<HttpClientWithUrl>,
}

impl DocsDotRsProvider {
    pub fn new(http_client: Arc<HttpClientWithUrl>) -> Self {
        Self { http_client }
    }
}

#[async_trait]
impl RustdocProvider for DocsDotRsProvider {
    async fn fetch_page(
        &self,
        crate_name: &PackageName,
        item: Option<&RustdocItem>,
    ) -> Result<Option<String>> {
        let version = "latest";
        let path = format!(
            "{crate_name}/{version}/{crate_name}{item_path}",
            item_path = item
                .map(|item| format!("/{}", item.url_path()))
                .unwrap_or_default()
        );

        let mut response = self
            .http_client
            .get(
                &format!("https://docs.rs/{path}"),
                AsyncBody::default(),
                true,
            )
            .await?;

        let mut body = Vec::new();
        response
            .body_mut()
            .read_to_end(&mut body)
            .await
            .context("error reading docs.rs response body")?;

        if response.status().is_client_error() {
            let text = String::from_utf8_lossy(body.as_slice());
            bail!(
                "status error {}, response: {text:?}",
                response.status().as_u16()
            );
        }

        Ok(Some(String::from_utf8(body)?))
    }
}
