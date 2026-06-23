pub(crate) const MODE_FIXED: u32 = 1;
pub(crate) const MODE_FIRST_PERSON: u32 = 2;
pub(crate) const MODE_CHASE: u32 = 3;
pub(crate) const MODE_CAMERAMAN: u32 = 4;

pub(crate) const DEFAULT_DISABLED_CAMERA_MASK: u32 = 1 << MODE_FIRST_PERSON;

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
            disabled_mask: DEFAULT_DISABLED_CAMERA_MASK,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParsedConfig {
    pub(crate) config: CameraConfig,
    pub(crate) warnings: Vec<String>,
}

pub(crate) fn parse_config(contents: &str) -> Result<ParsedConfig, String> {
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
            in_cameras = line == "[cameras]";
            continue;
        }

        if !in_cameras {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {}: expected key = value", line_index + 1));
        };
        let key = key.trim();
        let value = parse_bool(value.trim())
            .ok_or_else(|| format!("line {}: expected true or false", line_index + 1))?;

        match key {
            "fixed" | "point_camera" => config.fixed = value,
            "first_person" | "first-person" | "ineye" | "in_eye" => {
                config.first_person = value;
            }
            "chase" => config.chase = value,
            "cameraman" | "freecam" | "free_camera" => config.cameraman = value,
            "top" | "spawn" => warnings.push(format!(
                "config key '{key}' is not independently detectable yet; use fixed=false to disable this family"
            )),
            _ => warnings.push(format!("ignoring unknown config key '{key}'")),
        }
    }

    config.refresh_disabled_mask();
    Ok(ParsedConfig { config, warnings })
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

impl CameraConfig {
    fn refresh_disabled_mask(&mut self) {
        self.disabled_mask = 0;
        if !self.fixed {
            self.disabled_mask |= 1 << MODE_FIXED;
        }
        if !self.first_person {
            self.disabled_mask |= 1 << MODE_FIRST_PERSON;
        }
        if !self.chase {
            self.disabled_mask |= 1 << MODE_CHASE;
        }
        if !self.cameraman {
            self.disabled_mask |= 1 << MODE_CAMERAMAN;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CameraConfig, MODE_CAMERAMAN, MODE_CHASE, MODE_FIRST_PERSON, ParsedConfig, parse_config,
    };

    #[test]
    fn parses_camera_config() {
        let parsed = parse_config(
            r#"
            [cameras]
            first_person = false
            chase = false
            fixed = true
            cameraman = true
            "#,
        )
        .unwrap();

        assert_eq!(
            parsed,
            ParsedConfig {
                config: CameraConfig {
                    fixed: true,
                    first_person: false,
                    chase: false,
                    cameraman: true,
                    disabled_mask: (1 << MODE_FIRST_PERSON) | (1 << MODE_CHASE),
                },
                warnings: Vec::new(),
            }
        );
    }

    #[test]
    fn supports_camera_config_aliases() {
        let parsed = parse_config(
            r#"
            [cameras]
            in_eye = true
            free_camera = false
            "#,
        )
        .unwrap();

        assert_eq!(parsed.config.disabled_mask, 1 << MODE_CAMERAMAN);
    }

    #[test]
    fn keeps_defaults_for_empty_config() {
        assert_eq!(parse_config("").unwrap().config, CameraConfig::default());
    }

    #[test]
    fn default_config_matches_template_file() {
        let parsed = parse_config(include_str!("../autodirector-fix-config.toml")).unwrap();

        assert_eq!(parsed.config, CameraConfig::default());
        assert!(parsed.warnings.is_empty());
    }

    #[test]
    fn warns_about_currently_undetectable_camera_names() {
        let parsed = parse_config(
            r#"
            [cameras]
            top = false
            spawn = false
            "#,
        )
        .unwrap();

        assert_eq!(parsed.config, CameraConfig::default());
        assert_eq!(parsed.warnings.len(), 2);
    }
}
