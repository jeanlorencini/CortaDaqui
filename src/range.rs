use anyhow::{Context, Result, ensure};
use url::Url;

#[derive(Clone, Debug)]
pub struct YoutubeLink {
    pub id: String,
    pub start_ms: Option<i32>,
}

impl YoutubeLink {
    pub fn parse(text: &str) -> Result<Self> {
        let url = Url::parse(text.trim()).context("Cole um link de vídeo do YouTube.")?;
        ensure!(
            matches!(url.scheme(), "https" | "http")
                && url.port().is_none()
                && url.username().is_empty()
                && url.password().is_none(),
            "Cole um link de vídeo do YouTube."
        );
        let mut path = url.path().trim_matches('/').split('/');
        let id = match url.host_str() {
            Some("youtu.be") => path
                .next()
                .filter(|_| path.next().is_none())
                .map(str::to_owned),
            Some("youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com") => {
                match path.next() {
                    Some("watch") if path.next().is_none() => url
                        .query_pairs()
                        .find(|(key, _)| key == "v")
                        .map(|(_, id)| id.into_owned()),
                    Some("shorts" | "live" | "embed") => path
                        .next()
                        .filter(|_| path.next().is_none())
                        .map(str::to_owned),
                    _ => None,
                }
            }
            _ => None,
        }
        .filter(|id| {
            id.len() == 11
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        })
        .context("Esse endereço não identifica um vídeo do YouTube.")?;

        let start_ms = url
            .query_pairs()
            .find(|(key, _)| matches!(key.as_ref(), "t" | "start"))
            .and_then(|(_, value)| youtube_time(&value))
            .or_else(|| url.fragment()?.strip_prefix("t=").and_then(youtube_time));
        Ok(Self { id, start_ms })
    }

    pub fn canonical(&self) -> String {
        format!("https://www.youtube.com/watch?v={}", self.id)
    }
}

fn youtube_time(text: &str) -> Option<i32> {
    let (hours, rest) = match text.split_once('h') {
        Some((n, rest)) => (u64::from(n.parse::<u32>().ok()?) * 3600, rest),
        None => (0, text),
    };
    let (minutes, rest) = match rest.split_once('m') {
        Some((n, rest)) => (u64::from(n.parse::<u32>().ok()?) * 60, rest),
        None => (0, rest),
    };
    let seconds = if rest.is_empty() && !text.is_empty() {
        0
    } else {
        rest.strip_suffix('s').unwrap_or(rest).parse::<u32>().ok()?
    };
    i32::try_from((hours + minutes + u64::from(seconds)) * 1000).ok()
}

pub fn parse_time(text: &str) -> Option<i32> {
    let mut parts = text.trim().rsplit(':');
    let seconds = parts.next()?;
    let (whole, fraction) = seconds
        .split_once(['.', ','])
        .map_or((seconds, "0"), |(whole, fraction)| (whole, fraction));
    let seconds = decimal(whole)?;
    let millis = match fraction.len() {
        1..=3 => decimal(fraction)? * 10_u64.pow(3 - fraction.len() as u32),
        _ => return None,
    };
    let minutes = parts.next().map(decimal).unwrap_or(Some(0))?;
    let hours = parts.next().map(decimal);
    if parts.next().is_some()
        || (text.contains(':') && seconds >= 60)
        || (hours.is_some() && minutes >= 60)
    {
        return None;
    }
    let hours = hours.unwrap_or(Some(0))?;
    i32::try_from((hours * 3600 + minutes * 60 + seconds) * 1000 + millis).ok()
}

fn decimal(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse::<u32>().ok().map(u64::from)
}

pub fn clock(ms: i32) -> String {
    let clock = format!(
        "{:02}:{:02}:{:02}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60
    );
    let fraction = ms % 1000;
    if fraction == 0 {
        clock
    } else {
        format!("{clock}.{fraction:03}")
    }
}

pub fn seconds(ms: i32) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

#[derive(Clone, Copy, Debug)]
pub struct ClipRange {
    pub start: i32,
    pub end: i32,
}

impl ClipRange {
    pub fn from_text(start: &str, end: &str, duration: i32, frame_ms: i32) -> Result<Self> {
        let start = parse_time(start).context("Digite um início válido, como 00:01:35.000.")?;
        let end = parse_time(end).context("Digite um fim válido, como 00:02:10.000.")?;
        ensure!(start < end, "O fim precisa ser depois do início.");
        ensure!(end <= duration, "O fim ultrapassa a duração do vídeo.");
        ensure!(
            end - start >= frame_ms,
            "Selecione um trecho com pelo menos um quadro."
        );
        Ok(Self { start, end })
    }

    pub fn length(self) -> i32 {
        self.end - self.start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_links_keep_the_timestamp_and_drop_other_parameters() {
        for url in [
            "https://youtu.be/BaW_jenozKc?t=95",
            "https://www.youtube.com/watch?v=BaW_jenozKc&list=abc&t=1m35s",
            "https://m.youtube.com/shorts/BaW_jenozKc#t=95s",
        ] {
            let link = YoutubeLink::parse(url).unwrap();
            assert_eq!(link.start_ms, Some(95_000));
            assert_eq!(
                link.canonical(),
                "https://www.youtube.com/watch?v=BaW_jenozKc"
            );
        }
        assert!(YoutubeLink::parse("https://youtube.com.evil.test/watch?v=BaW_jenozKc").is_err());
        assert!(YoutubeLink::parse("https://youtube.com/playlist?list=BaW_jenozKc").is_err());
        assert_eq!(
            YoutubeLink::parse("https://youtu.be/BaW_jenozKc")
                .unwrap()
                .start_ms,
            None
        );
    }

    #[test]
    fn manual_times_have_millisecond_precision_and_a_valid_range() {
        assert_eq!(parse_time("1:35,125"), Some(95_125));
        assert_eq!(parse_time("95.1"), Some(95_100));
        assert_eq!(parse_time(&clock(29_799_001)), Some(29_799_001));
        for value in ["1:60", "-1", "1e2", "1:60:00", "1:4.1234", "0::1", "3."] {
            assert_eq!(parse_time(value), None, "{value}");
        }
        assert!(ClipRange::from_text("1", "1", 9000, 34).is_err());
        assert!(ClipRange::from_text("1", "9.1", 9000, 34).is_err());
        assert!(ClipRange::from_text("1", "1.001", 9000, 34).is_err());
        assert_eq!(
            ClipRange::from_text("1", "1.034", 9000, 34)
                .unwrap()
                .length(),
            34
        );
    }
}
