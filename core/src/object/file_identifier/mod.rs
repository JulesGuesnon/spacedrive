use crate::{
	invalidate_query,
	job::{JobError, JobReportUpdate, JobResult, WorkerContext},
	library::Library,
	location::file_path_helper::{file_path_for_file_identifier, FilePathError, MaterializedPath},
	object::{cas::generate_cas_id, object_for_file_identifier},
	prisma::{file_path, location, object, PrismaClient},
	sync,
	sync::SyncManager,
	util::db::uuid_to_bytes,
};

use sd_file_ext::{extensions::Extension, kind::ObjectKind};
use sd_sync::CRDTOperation;

use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
	collections::{HashMap, HashSet},
	path::{Path, PathBuf},
};
use thiserror::Error;
use tokio::{fs, io};
use tracing::{error, info};
use uuid::Uuid;

pub mod file_identifier_job;
pub mod shallow_file_identifier_job;

// we break these jobs into chunks of 100 to improve performance
const CHUNK_SIZE: usize = 100;

#[derive(Error, Debug)]
pub enum FileIdentifierJobError {
	#[error("File path related error (error: {0})")]
	FilePathError(#[from] FilePathError),
}

#[derive(Debug, Clone)]
pub struct FileMetadata {
	pub cas_id: String,
	pub kind: ObjectKind,
	pub fs_metadata: std::fs::Metadata,
}

impl FileMetadata {
	/// Assembles `create_unchecked` params for a given file path
	pub async fn new(
		location_path: impl AsRef<Path>,
		materialized_path: &MaterializedPath<'_>, // TODO: use dedicated CreateUnchecked type
	) -> Result<FileMetadata, io::Error> {
		let path = location_path.as_ref().join(materialized_path);

		let fs_metadata = fs::metadata(&path).await?;

		assert!(
			!fs_metadata.is_dir(),
			"We can't generate cas_id for directories"
		);

		// derive Object kind
		let kind = Extension::resolve_conflicting(&path, false)
			.await
			.map(Into::into)
			.unwrap_or(ObjectKind::Unknown);

		let cas_id = generate_cas_id(&path, fs_metadata.len()).await?;

		info!("Analyzed file: {path:?} {cas_id:?} {kind:?}");

		Ok(FileMetadata {
			cas_id,
			kind,
			fs_metadata,
		})
	}
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct FileIdentifierReport {
	location_path: PathBuf,
	total_orphan_paths: usize,
	total_objects_created: usize,
	total_objects_linked: usize,
	total_objects_ignored: usize,
}

async fn identifier_job_step(
	Library { db, sync, .. }: &Library,
	location: &location::Data,
	file_paths: &[file_path_for_file_identifier::Data],
) -> Result<(usize, usize), JobError> {
	let file_path_metas = join_all(file_paths.iter().map(|file_path| async move {
		// NOTE: `file_path`'s `materialized_path` begins with a `/` character so we remove it to join it with `location.path`
		FileMetadata::new(
			&location.path,
			&MaterializedPath::from((location.id, &file_path.materialized_path)),
		)
		.await
		.map(|params| {
			(
				Uuid::from_slice(&file_path.pub_id).unwrap(),
				(params, file_path),
			)
		})
	}))
	.await
	.into_iter()
	.flat_map(|data| {
		if let Err(e) = &data {
			error!("Error assembling Object metadata: {e}");
		}

		data
	})
	.collect::<HashMap<_, _>>();

	let unique_cas_ids = file_path_metas
		.values()
		.map(|(meta, _)| meta.cas_id.clone())
		.collect::<HashSet<_>>()
		.into_iter()
		.collect();

	// Assign cas_id to each file path
	sync.write_ops(
		db,
		file_path_metas
			.iter()
			.map(|(pub_id, (meta, _))| {
				(
					sync.shared_update(
						sync::file_path::SyncId {
							pub_id: uuid_to_bytes(*pub_id),
						},
						file_path::cas_id::NAME,
						json!(&meta.cas_id),
					),
					db.file_path().update(
						file_path::pub_id::equals(uuid_to_bytes(*pub_id)),
						vec![file_path::cas_id::set(Some(meta.cas_id.clone()))],
					),
				)
			})
			.unzip::<_, _, _, Vec<_>>(),
	)
	.await?;

	// Retrieves objects that are already connected to file paths with the same id
	let existing_objects = db
		.object()
		.find_many(vec![object::file_paths::some(vec![
			file_path::cas_id::in_vec(unique_cas_ids),
		])])
		.select(object_for_file_identifier::select())
		.exec()
		.await?;

	let existing_object_cas_ids = existing_objects
		.iter()
		.flat_map(|o| o.file_paths.iter().filter_map(|fp| fp.cas_id.as_ref()))
		.collect::<HashSet<_>>();

	// Attempt to associate each file path with an object that has been
	// connected to file paths with the same cas_id
	let updated_file_paths = sync
		.write_ops(
			db,
			file_path_metas
				.iter()
				.flat_map(|(pub_id, (meta, _))| {
					existing_objects
						.iter()
						.find(|o| {
							o.file_paths
								.iter()
								.any(|fp| fp.cas_id.as_ref() == Some(&meta.cas_id))
						})
						.map(|o| (*pub_id, o))
				})
				.map(|(pub_id, object)| {
					let (crdt_op, db_op) = file_path_object_connect_ops(
						pub_id,
						// SAFETY: This pub_id is generated by the uuid lib, but we have to store bytes in sqlite
						Uuid::from_slice(&object.pub_id).unwrap(),
						sync,
						db,
					);

					(crdt_op, db_op.select(file_path::select!({ pub_id })))
				})
				.unzip::<_, _, Vec<_>, Vec<_>>(),
		)
		.await?;

	info!(
		"Found {} existing Objects in Library, linking file paths...",
		existing_objects.len()
	);

	// extract objects that don't already exist in the database
	let file_paths_requiring_new_object = file_path_metas
		.into_iter()
		.filter(|(_, (meta, _))| !existing_object_cas_ids.contains(&meta.cas_id))
		.collect::<Vec<_>>();

	let total_created = if !file_paths_requiring_new_object.is_empty() {
		let new_objects_cas_ids = file_paths_requiring_new_object
			.iter()
			.map(|(_, (meta, _))| &meta.cas_id)
			.collect::<HashSet<_>>();

		info!(
			"Creating {} new Objects in Library... {:#?}",
			file_paths_requiring_new_object.len(),
			new_objects_cas_ids
		);

		let (object_create_args, file_path_update_args): (Vec<_>, Vec<_>) =
			file_paths_requiring_new_object
				.iter()
				.map(|(file_path_pub_id, (meta, fp))| {
					let object_pub_id = Uuid::new_v4();

					let sync_id = || sync::object::SyncId {
						pub_id: uuid_to_bytes(object_pub_id),
					};

					let kind = meta.kind as i32;

					let object_creation_args = (
						[sync.shared_create(sync_id())]
							.into_iter()
							.chain(
								[
									(object::date_created::NAME, json!(fp.date_created)),
									(object::kind::NAME, json!(kind)),
								]
								.into_iter()
								.map(|(f, v)| sync.shared_update(sync_id(), f, v)),
							)
							.collect::<Vec<_>>(),
						object::create_unchecked(
							uuid_to_bytes(object_pub_id),
							vec![
								object::date_created::set(fp.date_created),
								object::kind::set(kind),
							],
						),
					);

					(object_creation_args, {
						let (crdt_op, db_op) = file_path_object_connect_ops(
							*file_path_pub_id,
							object_pub_id,
							sync,
							db,
						);

						(crdt_op, db_op.select(file_path::select!({ pub_id })))
					})
				})
				.unzip();

		// create new object records with assembled values
		let total_created_files = sync
			.write_ops(db, {
				let (sync, db_params): (Vec<_>, Vec<_>) = object_create_args.into_iter().unzip();

				(sync.concat(), db.object().create_many(db_params))
			})
			.await
			.unwrap_or_else(|e| {
				error!("Error inserting files: {:#?}", e);
				0
			});

		info!("Created {} new Objects in Library", total_created_files);

		if total_created_files > 0 {
			info!("Updating file paths with created objects");

			sync.write_ops(db, {
				let data: (Vec<_>, Vec<_>) = file_path_update_args.into_iter().unzip();

				data
			})
			.await?;

			info!("Updated file paths with created objects");
		}

		total_created_files as usize
	} else {
		0
	};

	Ok((total_created, updated_file_paths.len()))
}

fn file_path_object_connect_ops<'db>(
	file_path_id: Uuid,
	object_id: Uuid,
	sync: &SyncManager,
	db: &'db PrismaClient,
) -> (CRDTOperation, file_path::UpdateQuery<'db>) {
	info!("Connecting <FilePath id={file_path_id}> to <Object pub_id={object_id}'>");

	let vec_id = object_id.as_bytes().to_vec();

	(
		sync.shared_update(
			sync::file_path::SyncId {
				pub_id: uuid_to_bytes(file_path_id),
			},
			file_path::object::NAME,
			json!(sync::object::SyncId {
				pub_id: vec_id.clone()
			}),
		),
		db.file_path().update(
			file_path::pub_id::equals(uuid_to_bytes(file_path_id)),
			vec![file_path::object::connect(object::pub_id::equals(vec_id))],
		),
	)
}

async fn process_identifier_file_paths(
	job_name: &str,
	location: &location::Data,
	file_paths: &[file_path_for_file_identifier::Data],
	step_number: usize,
	cursor: &mut i32,
	report: &mut FileIdentifierReport,
	ctx: WorkerContext,
) -> Result<(), JobError> {
	// if no file paths found, abort entire job early, there is nothing to do
	// if we hit this error, there is something wrong with the data/query
	if file_paths.is_empty() {
		return Err(JobError::EarlyFinish {
			name: job_name.to_string(),
			reason: "Expected orphan Paths not returned from database query for this chunk"
				.to_string(),
		});
	}

	info!(
		"Processing {:?} orphan Paths. ({} completed of {})",
		file_paths.len(),
		step_number,
		report.total_orphan_paths
	);

	let (total_objects_created, total_objects_linked) =
		identifier_job_step(&ctx.library, location, file_paths).await?;

	report.total_objects_created += total_objects_created;
	report.total_objects_linked += total_objects_linked;

	// set the step data cursor to the last row of this chunk
	if let Some(last_row) = file_paths.last() {
		*cursor = last_row.id;
	}

	ctx.progress(vec![
		JobReportUpdate::CompletedTaskCount(step_number),
		JobReportUpdate::Message(format!(
			"Processed {} of {} orphan Paths",
			step_number * CHUNK_SIZE,
			report.total_orphan_paths
		)),
	]);

	Ok(())
}

fn finalize_file_identifier(report: &FileIdentifierReport, ctx: WorkerContext) -> JobResult {
	info!("Finalizing identifier job: {report:?}");

	if report.total_orphan_paths > 0 {
		invalidate_query!(ctx.library, "locations.getExplorerData");
	}

	Ok(Some(serde_json::to_value(report)?))
}
