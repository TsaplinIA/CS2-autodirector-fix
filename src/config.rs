pub(crate) const MODE_FIXED: u32 = 1;
pub(crate) const MODE_FIRST_PERSON: u32 = 2;
pub(crate) const MODE_CHASE: u32 = 3;
pub(crate) const MODE_CAMERAMAN: u32 = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CameraConfig {
    pub(crate) fixed: bool,
    pub(crate) first_person: bool,
    pub(crate) chase: bool,
    pub(crate) cameraman: bool,
    pub(crate) disabled_mask: u32,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            fixed: true,
            first_person: false,
            chase: true,
            cameraman: true,
            disabled_mask: 1 << MODE_FIRST_PERSON,
        }
    }
}

pub(crate) fn parse_camera_config(contents: &str) -> Result<(CameraConfig, Vec<String>), String> {
    let mut config = CameraConfig::default();
    let mut warnings = Vec::new();
    let mut in_cameras = false;

    for (line_index, raw_line) in contents.lines().enumerate() {
        let line = raw_line
            .split_once('#')
            .map_or(raw_line, |(line, _)| line)
            .trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_cameras = &line[1..line.len() - 1] == "cameras";
            continue;
        }
        if !in_cameras {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {}: expected key = value", line_index + 1));
        };
        let value = match value.trim() {
            "true" => true,
            "false" => false,
            _ => return Err(format!("line {}: expected true or false", line_index + 1)),
        };
        match key.trim() {
            "fixed" | "point_camera" => config.fixed = value,
            "first_person" | "first-person" | "ineye" | "in_eye" => config.first_person = value,
            "chase" => config.chase = value,
            "cameraman" | "freecam" | "free_camera" => config.cameraman = value,
            key => warnings.push(format!("ignoring unknown cameras key '{key}'")),
        }
    }

    config.disabled_mask = 0;
    if !config.fixed {
        config.disabled_mask |= 1 << MODE_FIXED;
    }
    if !config.first_person {
        config.disabled_mask |= 1 << MODE_FIRST_PERSON;
    }
    if !config.chase {
        config.disabled_mask |= 1 << MODE_CHASE;
    }
    if !config.cameraman {
        config.disabled_mask |= 1 << MODE_CAMERAMAN;
    }
    Ok((config, warnings))
}

#[cfg(test)]
mod tests {
    use super::{MODE_CHASE, MODE_FIRST_PERSON, parse_camera_config};

    #[test]
    fn parses_camera_switches() {
        let (config, warnings) =
            parse_camera_config("[cameras]\nfirst_person = false\nchase = false").unwrap();
        assert_eq!(
            config.disabled_mask,
            (1 << MODE_FIRST_PERSON) | (1 << MODE_CHASE)
        );
        assert!(warnings.is_empty());
    }
}
