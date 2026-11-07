use cd_geometry::Rotation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XrandrMode {
    pub width: u32,
    pub height: u32,
    /// X position in the X screen coordinate space (native pixels).
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct XrandrOutput {
    pub name: String,
    pub connected: bool,
    pub primary: bool,
    pub mode: Option<XrandrMode>,
    pub physical_mm: Option<(f64, f64)>,
    pub rotation: Rotation,
}

/// Parse the output of `xrandr --current`.
///
/// Output headers look like:
/// `HDMI-0 connected primary 3840x2160+0+0 (normal ...) 631mm x 360mm`
pub fn parse_outputs(text: &str) -> Vec<XrandrOutput> {
    let mut outputs = Vec::new();
    for line in text.lines() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 2 {
            continue;
        }
        let connected = match tokens[1] {
            "connected" => true,
            "disconnected" => false,
            _ => continue,
        };
        let primary = tokens[2..].contains(&"primary");

        let mode = tokens.iter().find_map(|token| parse_mode(token));
        let physical_mm = parse_physical_mm(&tokens);
        let rotation = parse_rotation(line);

        outputs.push(XrandrOutput {
            name: tokens[0].to_owned(),
            connected,
            primary,
            mode,
            physical_mm,
            rotation,
        });
    }
    outputs
}

fn parse_mode(token: &str) -> Option<XrandrMode> {
    let parts: Vec<&str> = token.split('+').collect();
    if parts.len() != 3 {
        return None;
    }
    let (width, height) = parts[0].split_once('x')?;
    let width = width.parse::<u32>().ok()?;
    let height = height.parse::<u32>().ok()?;
    let x = parts[1].parse::<i32>().ok()?;
    let y = parts[2].parse::<i32>().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    Some(XrandrMode {
        width,
        height,
        x,
        y,
    })
}

fn parse_physical_mm(tokens: &[&str]) -> Option<(f64, f64)> {
    let mut values = tokens
        .iter()
        .filter(|token| token.ends_with("mm"))
        .filter_map(|token| token.trim_end_matches("mm").parse::<f64>().ok())
        .filter(|value| *value > 0.0);
    let width = values.next()?;
    let height = values.next()?;
    Some((width, height))
}

fn parse_rotation(line: &str) -> Rotation {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let boundary = tokens
        .iter()
        .position(|token| token.starts_with('('))
        .unwrap_or(tokens.len());
    for token in &tokens[..boundary] {
        match *token {
            "normal" => return Rotation::Deg0,
            "left" => return Rotation::Deg90,
            "inverted" => return Rotation::Deg180,
            "right" => return Rotation::Deg270,
            _ => {}
        }
    }
    Rotation::Deg0
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
Screen 0: minimum 8 x 8, current 3840 x 2160, maximum 32767 x 32767
HDMI-0 connected primary 3840x2160+0+0 (normal left inverted right x axis y axis) 631mm x 360mm
   3840x2160     60.00*+  60.00    59.94    50.00    29.97    25.00    23.98
   2560x1440     59.95
DP-0 disconnected (normal left inverted right x axis y axis)
DP-1 connected 1920x1080+1920+360 left (normal left inverted right x axis y axis) 344mm x 194mm
   1920x1080     60.00*+
";

    #[test]
    fn parses_connected_outputs_and_ignores_modes() {
        let outputs = parse_outputs(SAMPLE);
        assert_eq!(outputs.len(), 3);
        assert!(outputs[0].connected);
        assert!(outputs[0].primary);
        assert_eq!(
            outputs[0].mode,
            Some(XrandrMode {
                width: 3840,
                height: 2160,
                x: 0,
                y: 0
            })
        );
        assert_eq!(outputs[0].physical_mm, Some((631.0, 360.0)));
        assert_eq!(outputs[0].rotation, Rotation::Deg0);

        assert!(!outputs[1].connected);
        assert!(outputs[1].mode.is_none());

        assert!(outputs[2].connected);
        assert!(!outputs[2].primary);
        assert_eq!(outputs[2].rotation, Rotation::Deg90);
    }

    #[test]
    fn parses_negative_origins() {
        let line =
            "DP-1 connected 1920x1080+-1920+0 (normal left inverted right x axis y axis) 344mm x 194mm";
        let outputs = parse_outputs(line);
        assert_eq!(
            outputs[0].mode,
            Some(XrandrMode {
                width: 1920,
                height: 1080,
                x: -1920,
                y: 0
            })
        );
    }

    #[test]
    fn missing_physical_size_is_tolerated() {
        let line = "eDP-1 connected 2560x1600+0+0 (normal left inverted right x axis y axis)";
        let outputs = parse_outputs(line);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].physical_mm, None);
    }
}
