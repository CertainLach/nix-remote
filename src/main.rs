use std::{
	collections::BTreeSet,
	ffi::OsStr,
	os::unix::prelude::{OsStrExt, PermissionsExt},
	path::{Path, PathBuf},
	process::{exit, ExitCode},
};

use anyhow::{anyhow, bail, Context, Result};
use openssh::{KnownHosts, Session, Stdio};
use openssh_sftp_client::{Sftp, SftpOptions};
use serde::Deserialize;
use tokio::{fs, process::Command};
use tracing::{error, info, warn};

use clap::Parser;

#[derive(Parser)]
struct Opts {
	installable: String,
	ssh: String,
	// Deduce automatically from installable main attribute?
	#[clap(short = 'c')]
	command: String,
}

#[derive(Deserialize, Debug)]
struct ClosurePath {
	path: String,
}

// pub const FULL_STORE_PREFIX_LEN: usize = "/nix/store/004b0bvpjng4l23kahn6vzawlpr6dx75-".len();
pub const NIX_STORE: &str = "/nix/store/";
pub const DEFAULT_REMAP: &str = "/tmp/nixrm/";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<ExitCode> {
	tracing_subscriber::fmt::fmt().without_time().init();

	let opts = Opts::parse();

	info!("building...");
	let code = Command::new("nix")
		.args(["build", "--no-link"])
		.arg(&opts.installable)
		.spawn()?
		.wait()
		.await?;
	if !code.success() {
		error!("build failed");
		exit(1);
	}
	info!("loading closure");
	let paths = {
		let output = Command::new("nix")
			.args(["path-info", "--json", "-r"])
			.arg(&opts.installable)
			.stdout(Stdio::inherit())
			.output()
			.await?;
		if !output.status.success() {
			error!("closure query failed");
			exit(1);
		}
		let paths: Vec<ClosurePath> = serde_json::from_slice(&output.stdout)?;
		paths
	};
	let primary_path = {
		let output = Command::new("nix")
			.args(["path-info", "--json"])
			.arg(&opts.installable)
			.stdout(Stdio::inherit())
			.output()
			.await?;
		if !output.status.success() {
			error!("path query failed");
			exit(1);
		}
		let paths: Vec<ClosurePath> = serde_json::from_slice(&output.stdout)?;
		assert!(
			paths.len() == 1,
			"should exist, otherwise closure query will fail"
		);
		paths.into_iter().next().unwrap()
	};
	// dbg!(&paths);
	let paths = paths.into_iter().map(|p| p.path).collect::<Vec<_>>();
	let paths_regex = paths
		.iter()
		.map(|p| regex::escape(p))
		.collect::<Vec<_>>()
		.join("|");
	let paths_regex = regex::bytes::Regex::new(&paths_regex).expect("escaped");
	let paths = paths
		.into_iter()
		.map(|p| {
			p.strip_prefix(NIX_STORE)
				.expect("all prefixed with {NIX_STORE}")
				.to_owned()
		})
		.collect::<BTreeSet<_>>();

	info!("closure contains {} paths", paths.len());

	info!("initializing SSH");
	let session = Session::connect(&opts.ssh, KnownHosts::Strict).await?;
	let output = session
		.command("mkdir")
		.arg("-p")
		.arg(DEFAULT_REMAP)
		.status()
		.await?;
	if !output.success() {
		error!("failed to create store remap");
		exit(1);
	}

	let mut sftp = session
		.subsystem("sftp")
		.stdin(openssh::Stdio::piped())
		.stdout(openssh::Stdio::piped())
		.spawn()
		.await?;
	let sftp = Sftp::new(
		sftp.stdin().take().expect("piped"),
		sftp.stdout().take().expect("piped"),
		SftpOptions::new(),
	)
	.await?;
	let mut fs = sftp.fs();
	// FIXME: possible vulnerability, anyone can edit root directory itself
	// ideally this should be per-user directory, maybe in XDG_RUNTIME_DIR
	let _ = fs.dir_builder().create(DEFAULT_REMAP).await;

	let installed_dir = {
		let mut out = PathBuf::from(DEFAULT_REMAP);
		out.push("installed");
		out
	};
	let _ = fs.dir_builder().create(&installed_dir).await;

	let existing = {
		let mut marker_dir = fs.open_dir(&installed_dir).await?;
		info!("querying existing paths");

		let existing = marker_dir.read_dir().await?;
		let existing = existing
			.into_iter()
			.filter(|e| e.filename().to_str() != Some("installed"))
			.map(|e| {
				e.filename()
					.to_str()
					.ok_or_else(|| anyhow!("bad name in store dir"))
					.map(|s| s.to_owned())
			})
			.collect::<Result<BTreeSet<_>>>()?;
		marker_dir.close().await?;
		existing
	};

	let remap_path = |src: &Path| -> Result<PathBuf> {
		// TODO: support DEFAULT_REMAP with length different from NIX_STORE
		let src = src.strip_prefix(NIX_STORE)?;
		let mut remapped = PathBuf::from(DEFAULT_REMAP);
		remapped.push(src);
		Ok(remapped)
	};

	// TODO: make it atomic/locking
	// TODO: All sftp communication is sketchy, and works poorly, maybe the helper program will help?
	for path in paths.difference(&existing) {
		info!("installing {path}");
		let mut local_path = PathBuf::from(NIX_STORE);
		local_path.push(path);

		{
			let remote_path = remap_path(&local_path)?;
			if fs.metadata(&remote_path).await.is_ok() {
				warn!("path exists, that is unexpected, removing");
				let o = session
					.command("rm")
					.arg("-rf")
					.arg(
						remote_path
							.to_str()
							.ok_or_else(|| anyhow!("no support for non-utf8 paths"))?,
					)
					.status()
					.await?;
				if !o.success() {
					bail!("rm failed for {path:?}");
				}
			}
		}

		let mut permissions = Vec::new();
		for entry in walkdir::WalkDir::new(&local_path) {
			let entry = entry?;
			let mut remote_entry_path = PathBuf::from(DEFAULT_REMAP);
			remote_entry_path.push(entry.path().strip_prefix(NIX_STORE).expect("in nix store"));
			info!("processing {remote_entry_path:?}");

			let metadata = entry.metadata()?;
			if metadata.is_dir() {
				fs.dir_builder()
					.create(&remote_entry_path)
					.await
					.with_context(|| format!("mkdir failed at {remote_entry_path:?}"))?;
				permissions.push((remote_entry_path.clone(), metadata.permissions().mode()));
			} else if metadata.is_file() {
				let mut remote_file = fs
					.sftp()
					.options()
					.create_new(true)
					.write(true)
					// FIXME: there is fileattrs, but they are not exposed in public api
					.open(&remote_entry_path)
					.await
					.with_context(|| format!("create failed at {remote_entry_path:?}"))?;

				let local_file = std::fs::File::open(entry.path())?;
				if local_file.metadata()?.len() == 0 {
					remote_file.close().await?;
					continue;
				}
				let local_file = unsafe { memmap::Mmap::map(&local_file) }?;
				let mut local_file = &local_file as &[u8];
				while !local_file.is_empty() {
					if let Some(pos) = paths_regex.find(local_file) {
						if pos.start() != 0 {
							remote_file.write_all(&local_file[..pos.start()]).await?;
						}
						let path = PathBuf::from(OsStr::from_bytes(pos.as_bytes()));
						let remapped = remap_path(&path)?;
						remote_file
							.write_all(remapped.as_os_str().as_bytes())
							.await?;
						local_file = &local_file[pos.end()..];
					} else {
						remote_file.write_all(local_file).await?;
						local_file = &[];
					}
				}
				remote_file.close().await?;
				let o = session
					.command("chmod")
					.arg(format!("{:0>3o}", metadata.permissions().mode() & 0o777))
					.arg(
						remote_entry_path
							.to_str()
							.ok_or_else(|| anyhow!("no support for non-utf8 paths"))?,
					)
					.status()
					.await?;
				if !o.success() {
					bail!("chmod failed for {path:?}");
				}
				permissions.push((remote_entry_path.clone(), metadata.permissions().mode()));
			} else {
				let link = fs::read_link(entry.path()).await?;
				let remapped = if link.is_absolute() {
					remap_path(&link)?
				} else {
					link.to_path_buf()
				};
				// TODO: sftp api provided by openssh_sftp_client disallows creation of bad symlinks
				let o = session
					.command("ln")
					.arg("-s")
					.arg(
						remapped
							.to_str()
							.ok_or_else(|| anyhow!("no support for non-utf8 paths"))?,
					)
					.arg(
						remote_entry_path
							.to_str()
							.ok_or_else(|| anyhow!("no support for non-utf8 paths"))?,
					)
					.status()
					.await?;
				if !o.success() {
					bail!("ln failed for {remote_entry_path:?}");
				}
			}
		}
		for (path, mode) in permissions {
			let o = session
				.command("chmod")
				.arg(format!("{:0>3o}", mode & 0o777))
				.arg(
					path.to_str()
						.ok_or_else(|| anyhow!("no support for non-utf8 paths"))?,
				)
				.status()
				.await?;
			if !o.success() {
				bail!("chmod failed for {path:?}");
			}
		}
		{
			info!("finalizing");
			let mut installed = installed_dir.clone();
			installed.push(path);
			fs.write(&installed, &[]).await?;
		}
	}

	info!("done");

	let exec_err = exec::Command::new("ssh")
		.arg("-t")
		.arg(opts.ssh)
		.arg(format!(
			"export PATH=\"{}/bin:$PATH\"; {}",
			remap_path(&PathBuf::from(primary_path.path))?
				.to_str()
				.expect("copy will fail if path is not utf-8"),
			opts.command
		))
		.exec();
	Err(exec_err.into())
}
