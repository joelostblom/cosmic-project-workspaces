use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    io::Write,
    ops::Range,
    path::{Path, PathBuf},
    time::Duration,
};

const CONFIG_KEY: &str = "cosmic/com.system76.CosmicComp/v1/workspaces";
// COSMIC reloads the file asynchronously; there is no acknowledgement.
pub const CONFIG_SETTLE: Duration = Duration::from_millis(60);
// COSMIC 1.11 uses a 200 ms shortcut animation and reads the axis each frame.
pub const ANIMATION_HOLD: Duration = Duration::from_millis(230);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlideAxis {
    Vertical,
    Horizontal,
}

impl SlideAxis {
    fn as_str(self) -> &'static str {
        match self {
            Self::Vertical => "Vertical",
            Self::Horizontal => "Horizontal",
        }
    }
}

pub struct SlideExperiment(PathBuf);

impl SlideExperiment {
    pub fn new(config_home: &Path) -> Result<Self> {
        let path = config_home.join(CONFIG_KEY);
        let (config, _) = read_config(&path).context(
            "the slide experiment requires a user workspace setting; select a layout in COSMIC Settings first",
        )?;
        axis_span(&config)
            .with_context(|| format!("read workspace layout in {}", path.display()))?;
        Ok(Self(path))
    }

    pub fn apply(&self, axis: SlideAxis) -> Result<Option<LayoutOverride>> {
        let (original, permissions) = read_config(&self.0)?;
        let (span, current) = axis_span(&original)?;
        if current == axis {
            return Ok(None);
        }

        let mut temporary = original.clone();
        temporary.replace_range(span, axis.as_str());
        replace_config(&self.0, &original, &temporary, permissions)?;
        Ok(Some(LayoutOverride {
            path: self.0.clone(),
            original,
            temporary,
        }))
    }
}

/// Restores the original file on success or after an early return. A kill that
/// bypasses destructors cannot be recovered here.
pub struct LayoutOverride {
    path: PathBuf,
    original: String,
    temporary: String,
}

impl Drop for LayoutOverride {
    fn drop(&mut self) {
        let restored = read_config(&self.path).and_then(|(current, permissions)| {
            ensure!(
                current == self.temporary,
                "{} changed while the override was active; leaving it alone",
                self.path.display()
            );
            replace_config(&self.path, &self.temporary, &self.original, permissions)
        });
        if let Err(error) = restored {
            eprintln!("Could not restore COSMIC workspace layout: {error:#}");
        }
    }
}

fn read_config(path: &Path) -> Result<(String, fs::Permissions)> {
    let info = fs::symlink_metadata(path).with_context(|| format!("open {}", path.display()))?;
    ensure!(
        info.is_file(),
        "{} is not a regular config file",
        path.display()
    );
    let contents = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok((contents, info.permissions()))
}

fn replace_config(
    path: &Path,
    expected: &str,
    contents: &str,
    permissions: fs::Permissions,
) -> Result<()> {
    let parent = path
        .parent()
        .context("workspace config has no parent directory")?;
    let mut file = tempfile::Builder::new()
        .prefix(".atomicwrite-cpw-")
        .tempfile_in(parent)?;
    file.write_all(contents.as_bytes())?;
    file.as_file().set_permissions(permissions)?;
    ensure!(
        read_config(path)?.0 == expected,
        "{} was edited concurrently; not overwriting it",
        path.display()
    );
    file.persist(path)
        .with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// Match the single field as written by COSMIC, without reserializing the RON
/// or discarding other settings. Reject block comments and unknown formats.
fn axis_span(config: &str) -> Result<(Range<usize>, SlideAxis)> {
    ensure!(
        !config.contains("/*"),
        "unsupported workspace config: block comment"
    );
    let mut offset = 0;
    let mut field = None;
    for line in config.split_inclusive('\n') {
        let code = line.split_once("//").map_or(line, |(code, _)| code);
        let trimmed = code.trim_start();
        if let Some(value) = trimmed.strip_prefix("workspace_layout:") {
            let value = value.trim_start();
            let token = value.trim_end().trim_end_matches(',').trim_end();
            let axis = match token {
                "Horizontal" => SlideAxis::Horizontal,
                "Vertical" => SlideAxis::Vertical,
                _ => bail!("unrecognized workspace_layout value"),
            };
            ensure!(field.is_none(), "more than one workspace_layout field");
            let start = offset + code.len() - value.len();
            field = Some((start..start + token.len(), axis));
        }
        offset += line.len();
    }
    field.context("no standalone workspace_layout field; select a layout in COSMIC Settings first")
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERTICAL: &str = "(\n    workspace_mode: OutputBound,\n    workspace_layout: Vertical, // keep comment\n    action_on_typing: r#None,\n    workspace_wraparound: false,\n)";

    fn fixture() -> (tempfile::TempDir, SlideExperiment, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(CONFIG_KEY);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, VERTICAL).unwrap();
        let slide = SlideExperiment::new(root.path()).unwrap();
        (root, slide, path)
    }

    #[test]
    fn overrides_only_the_axis_and_restores_original_bytes() {
        let (_root, slide, path) = fixture();
        assert!(slide.apply(SlideAxis::Vertical).unwrap().is_none());
        let active = slide.apply(SlideAxis::Horizontal).unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            VERTICAL.replace("Vertical", "Horizontal")
        );
        drop(active);
        assert_eq!(fs::read_to_string(&path).unwrap(), VERTICAL);
    }

    #[test]
    fn does_not_restore_over_external_edits() {
        let (_root, slide, path) = fixture();
        let active = slide.apply(SlideAxis::Horizontal).unwrap().unwrap();
        let external = VERTICAL.replace("false", "true");
        fs::write(&path, &external).unwrap();
        drop(active);
        assert_eq!(fs::read_to_string(&path).unwrap(), external);
    }

    #[test]
    fn rejects_missing_or_ambiguous_axes() {
        assert!(axis_span("(\n // workspace_layout: Vertical,\n)").is_err());
        assert!(
            axis_span("(\n workspace_layout: Vertical,\n workspace_layout: Horizontal,\n)")
                .is_err()
        );
        assert!(axis_span("(\n workspace_layout: Other,\n)").is_err());
    }
}
