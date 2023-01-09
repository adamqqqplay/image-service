// Copyright 2020 Ant Group. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::fs::DirEntry;
use std::io::Write;

use anyhow::{Context, Result};

use crate::builder::Builder;
use crate::core::blob::Blob;
use crate::core::bootstrap::Bootstrap;
use crate::core::context::{
    ArtifactWriter, BlobManager, BootstrapContext, BootstrapManager, BuildContext, BuildOutput,
};
use crate::core::node::{Node, Overlay};
use crate::core::tree::Tree;

const TAR_BLOB_NAME: &str = "image.blob";
const TAR_BOOTSTRAP_NAME: &str = "image.boot";

struct FilesystemTreeBuilder {}

impl FilesystemTreeBuilder {
    fn new() -> Self {
        Self {}
    }

    #[allow(clippy::only_used_in_recursion)]
    /// Walk directory to build node tree by DFS
    fn load_children(
        &self,
        ctx: &mut BuildContext,
        bootstrap_ctx: &mut BootstrapContext,
        parent: &mut Node,
        layer_idx: u16,
    ) -> Result<Vec<Tree>> {
        let mut result = Vec::new();
        if !parent.is_dir() {
            return Ok(result);
        }

        let children = fs::read_dir(parent.path())
            .with_context(|| format!("failed to read dir {:?}", parent.path()))?;
        let children = children.collect::<Result<Vec<DirEntry>, std::io::Error>>()?;

        event_tracer!("load_from_directory", +children.len());
        for child in children {
            let path = child.path();
            let mut child = Node::new(
                ctx.fs_version,
                ctx.source_path.clone(),
                path.clone(),
                Overlay::UpperAddition,
                ctx.chunk_size,
                parent.explicit_uidgid,
                true,
            )
            .with_context(|| format!("failed to create node {:?}", path))?;
            child.layer_idx = layer_idx;

            // as per OCI spec, whiteout file should not be present within final image
            // or filesystem, only existed in layers.
            if !bootstrap_ctx.layered
                && child.whiteout_type(ctx.whiteout_spec).is_some()
                && !child.is_overlayfs_opaque(ctx.whiteout_spec)
            {
                continue;
            }

            let mut child = Tree::new(child);
            child.children = self.load_children(ctx, bootstrap_ctx, &mut child.node, layer_idx)?;
            child.node.v5_set_dir_size(ctx.fs_version, &child.children);
            result.push(child);
        }

        Ok(result)
    }
}

pub(crate) struct DirectoryBuilder {}

impl DirectoryBuilder {
    pub fn new() -> Self {
        Self {}
    }

    /// Build node tree from a filesystem directory
    fn build_tree_from_fs(
        &mut self,
        ctx: &mut BuildContext,
        bootstrap_ctx: &mut BootstrapContext,
        layer_idx: u16,
    ) -> Result<Tree> {
        let node = Node::new(
            ctx.fs_version,
            ctx.source_path.clone(),
            ctx.source_path.clone(),
            Overlay::UpperAddition,
            ctx.chunk_size,
            ctx.explicit_uidgid,
            true,
        )?;
        let mut tree = Tree::new(node);
        let tree_builder = FilesystemTreeBuilder::new();

        tree.children = timing_tracer!(
            { tree_builder.load_children(ctx, bootstrap_ctx, &mut tree.node, layer_idx) },
            "load_from_directory"
        )?;
        tree.node.v5_set_dir_size(ctx.fs_version, &tree.children);

        Ok(tree)
    }
}

impl Builder for DirectoryBuilder {
    fn build(
        &mut self,
        ctx: &mut BuildContext,
        bootstrap_mgr: &mut BootstrapManager,
        blob_mgr: &mut BlobManager,
    ) -> Result<BuildOutput> {
        let mut bootstrap_ctx = bootstrap_mgr.create_ctx(ctx.inline_bootstrap)?;
        let layer_idx = u16::from(bootstrap_ctx.layered);
        // Scan source directory to build upper layer tree.
        let mut tree = self.build_tree_from_fs(ctx, &mut bootstrap_ctx, layer_idx)?;

        let origin_bootstarp_offset = bootstrap_ctx.offset;
        let mut bootstrap = Bootstrap::new()?;
        if bootstrap_ctx.layered {
            // Merge with lower layer if there's one, do not prepare `prefetch` list during merging.
            ctx.prefetch.disable();
            bootstrap.build(ctx, &mut bootstrap_ctx, &mut tree)?;
            tree = bootstrap.apply(ctx, &mut bootstrap_ctx, bootstrap_mgr, blob_mgr, None)?;
        }
        // If layered, the bootstrap_ctx.offset will be set in first build, so we need restore it here
        bootstrap_ctx.offset = origin_bootstarp_offset;
        bootstrap_ctx.layered = false;

        // Convert the hierarchy tree into an array, stored in `bootstrap_ctx.nodes`.
        timing_tracer!(
            { bootstrap.build(ctx, &mut bootstrap_ctx, &mut tree) },
            "build_bootstrap"
        )?;

        let mut blob_writer = if let Some(blob_stor) = ctx.blob_storage.clone() {
            Some(ArtifactWriter::new(blob_stor, ctx.inline_bootstrap)?)
        } else {
            None
        };

        // Dump blob file
        let mut blob = Blob::new();
        timing_tracer!(
            { blob.dump(ctx, &mut bootstrap_ctx.nodes, blob_mgr, &mut blob_writer) },
            "dump_blob"
        )?;

        if let Some((_, blob_ctx)) = blob_mgr.get_current_blob() {
            if let Some(blob_writer) = &mut blob_writer {
                if ctx.inline_bootstrap {
                    blob_writer.write_tar_header(TAR_BLOB_NAME, blob_writer.pos()?)?;
                } else {
                    blob_writer.finalize(blob_ctx.blob_id())?;
                }
            }
        }

        // Dump bootstrap file
        let blob_table = blob_mgr.to_blob_table(ctx)?;
        bootstrap.dump(ctx, &mut bootstrap_ctx, &blob_table)?;

        if ctx.inline_bootstrap {
            if let Some(blob_writer) = &mut blob_writer {
                let bootstrap_data = bootstrap_ctx.writer.data();
                blob_writer.write_all(bootstrap_data)?;
                blob_writer.write_tar_header(TAR_BOOTSTRAP_NAME, bootstrap_data.len() as u64)?;
                blob_writer.finalize(Some(ctx.blob_id.clone()))?;
            }
        }

        BuildOutput::new(blob_mgr)
    }
}
