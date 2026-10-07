/// Keep the lines Discord expects in a WebRTC Select Protocol `data` fragment.
pub fn select_protocol_fragment(local_sdp: &str, vp8_rtx_payload: Option<u8>) -> String {
    let mut kept = Vec::new();
    for line in local_sdp.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let keep = line.starts_with("a=extmap-allow-mixed")
            || line.starts_with("a=ice-")
            || line.starts_with("a=fingerprint")
            || line.starts_with("a=extmap:")
            || rtpmap_is(line, "opus")
            || rtpmap_is(line, "VP8")
            || vp8_rtx_payload.is_some_and(|payload| {
                line.starts_with(&format!("a=rtpmap:{payload} rtx/"))
            });
        if keep && !kept.iter().any(|existing| existing == line) {
            kept.push(line.to_owned());
        }
    }
    kept.join("\n")
}

fn rtpmap_is(line: &str, codec: &str) -> bool {
    let Some(rest) = line.strip_prefix("a=rtpmap:") else {
        return false;
    };
    rest.split_whitespace()
        .nth(1)
        .is_some_and(|value| value.starts_with(&format!("{codec}/")))
}

#[derive(Debug, Clone)]
pub struct CodecAdvertisement {
    pub name: &'static str,
    pub kind: &'static str,
    pub priority: u32,
    pub payload_type: u8,
    pub rtx_payload_type: Option<u8>,
    pub encode: bool,
    pub decode: bool,
}

/// Opus plus H.264 decode. Other codecs are advertised only when the offer has them,
/// and only H.264 is marked decodable so AV1/H.265 frames are not requested.
pub fn advertisements_from_offer(local_sdp: &str) -> Vec<CodecAdvertisement> {
    let opus = payload_of(local_sdp, "opus").unwrap_or(111);
    let h264 = payload_of(local_sdp, "H264").unwrap_or(102);
    let h264_rtx = rtx_for(local_sdp, h264);
    let mut codecs = vec![
        CodecAdvertisement {
            name: "opus",
            kind: "audio",
            priority: 1000,
            payload_type: opus,
            rtx_payload_type: None,
            encode: false,
            decode: true,
        },
        CodecAdvertisement {
            name: "H264",
            kind: "video",
            priority: 1000,
            payload_type: h264,
            rtx_payload_type: h264_rtx,
            encode: false,
            decode: true,
        },
    ];
    for (name, priority) in [("H265", 2000u32), ("AV1", 3000), ("VP8", 4000), ("VP9", 5000)] {
        if let Some(payload) = payload_of(local_sdp, name) {
            codecs.push(CodecAdvertisement {
                name: match name {
                    "H265" => "H265",
                    "AV1" => "AV1",
                    "VP8" => "VP8",
                    "VP9" => "VP9",
                    _ => "H264",
                },
                kind: "video",
                priority,
                payload_type: payload,
                rtx_payload_type: rtx_for(local_sdp, payload),
                encode: false,
                decode: false,
            });
        }
    }
    codecs
}

pub fn payload_of(sdp: &str, codec: &str) -> Option<u8> {
    sdp.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("a=rtpmap:")?;
        let mut parts = rest.split_whitespace();
        let payload = parts.next()?.parse().ok()?;
        let name = parts.next()?;
        name.starts_with(&format!("{codec}/")).then_some(payload)
    })
}

fn rtx_for(sdp: &str, apt: u8) -> Option<u8> {
    let needle = format!("apt={apt}");
    sdp.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("a=fmtp:")?;
        if !rest.contains(&needle) {
            return None;
        }
        rest.split_whitespace().next()?.parse().ok()
    })
}

#[derive(Debug, Clone)]
pub struct TransportParams {
    pub ice_ufrag: String,
    pub ice_pwd: String,
    pub fingerprint: String,
    pub candidates: Vec<String>,
    pub connection: String,
}

pub fn transport_from_server_sdp(sdp: &str) -> Option<TransportParams> {
    let mut ice_ufrag = None;
    let mut ice_pwd = None;
    let mut fingerprint = None;
    let mut candidates = Vec::new();
    let mut connection = None;
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("a=ice-ufrag:") {
            ice_ufrag = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("a=ice-pwd:") {
            ice_pwd = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("a=fingerprint:") {
            fingerprint = Some(value.to_owned());
        } else if line.starts_with("a=candidate:") {
            candidates.push(line.trim_start_matches("a=").to_owned());
        } else if line.starts_with("c=") && connection.is_none() {
            connection = Some(line.to_owned());
        }
    }
    Some(TransportParams {
        ice_ufrag: ice_ufrag?,
        ice_pwd: ice_pwd?,
        fingerprint: fingerprint?,
        candidates,
        connection: connection.unwrap_or_else(|| "c=IN IP4 0.0.0.0".into()),
    })
}

#[derive(Debug, Clone)]
struct OfferedSection {
    kind: String,
    mid: String,
    direction: String,
    payloads: Vec<String>,
}

/// Build an answer with the same m-line count as the local offer.
pub fn synthesize_answer(local_offer: &str, server_sdp: &str, remote_video_ssrcs: &[u32]) -> Option<String> {
    let transport = transport_from_server_sdp(server_sdp)?;
    let sections = offered_sections(local_offer);
    if sections.is_empty() {
        return None;
    }
    let mids: Vec<&str> = sections.iter().map(|section| section.mid.as_str()).collect();
    let mut out = String::new();
    out.push_str("v=0\r\n");
    out.push_str("o=- 0 0 IN IP4 127.0.0.1\r\n");
    out.push_str("s=-\r\n");
    out.push_str("t=0 0\r\n");
    out.push_str(&format!("a=group:BUNDLE {}\r\n", mids.join(" ")));
    out.push_str("a=msid-semantic: WMS *\r\n");
    out.push_str(&format!("a=ice-ufrag:{}\r\n", transport.ice_ufrag));
    out.push_str(&format!("a=ice-pwd:{}\r\n", transport.ice_pwd));
    out.push_str(&format!("a=fingerprint:{}\r\n", transport.fingerprint));
    out.push_str("a=ice-lite\r\n");
    for candidate in &transport.candidates {
        out.push_str(&format!("a={candidate}\r\n"));
    }
    let mut video_index = 0usize;
    for section in &sections {
        let payloads = if section.payloads.is_empty() {
            "0".to_owned()
        } else {
            section.payloads.join(" ")
        };
        out.push_str(&format!("m={} 9 UDP/TLS/RTP/SAVPF {payloads}\r\n", section.kind));
        out.push_str(&format!("{}\r\n", transport.connection));
        out.push_str("a=rtcp:9 IN IP4 0.0.0.0\r\n");
        out.push_str("a=setup:passive\r\n");
        out.push_str(&format!("a=mid:{}\r\n", section.mid));
        out.push_str("a=rtcp-mux\r\n");
        let direction = answer_direction(&section.direction, section.kind == "video" && video_index < remote_video_ssrcs.len());
        out.push_str(&format!("a={direction}\r\n"));
        for line in local_offer.lines() {
            let line = line.trim();
            if line.starts_with("a=rtpmap:") || line.starts_with("a=fmtp:") || line.starts_with("a=rtcp-fb:") {
                if section.payloads.iter().any(|payload| line.contains(&format!(":{payload} ")) || line.contains(&format!(":{payload}\r")))
                    || section.payloads.iter().any(|payload| {
                        line.strip_prefix("a=rtpmap:").or_else(|| line.strip_prefix("a=fmtp:")).or_else(|| line.strip_prefix("a=rtcp-fb:"))
                            .is_some_and(|rest| rest.starts_with(payload))
                    })
                {
                    out.push_str(line);
                    out.push_str("\r\n");
                }
            }
        }
        if section.kind == "audio" {
            if let Some(opus) = section.payloads.first() {
                if !out.contains(&format!("a=fmtp:{opus}")) {
                    out.push_str(&format!("a=fmtp:{opus} minptime=10;useinbandfec=1\r\n"));
                }
                out.push_str("a=maxptime:60\r\n");
            }
        }
        if section.kind == "video" {
            if let Some(ssrc) = remote_video_ssrcs.get(video_index) {
                out.push_str(&format!("a=ssrc:{ssrc} cname:discord-omt\r\n"));
            }
            video_index += 1;
        }
    }
    Some(out)
}

fn answer_direction(offer: &str, has_remote: bool) -> &'static str {
    match (offer, has_remote) {
        ("recvonly", true) => "sendonly",
        ("recvonly", false) => "inactive",
        ("sendonly", _) => "recvonly",
        ("sendrecv", true) => "sendrecv",
        ("sendrecv", false) => "recvonly",
        _ => "inactive",
    }
}

fn offered_sections(sdp: &str) -> Vec<OfferedSection> {
    let mut sections = Vec::new();
    let mut current: Option<OfferedSection> = None;
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            if let Some(section) = current.take() {
                sections.push(section);
            }
            let mut parts = rest.split_whitespace();
            let kind = parts.next().unwrap_or("audio").to_owned();
            let _port = parts.next();
            let _proto = parts.next();
            current = Some(OfferedSection {
                kind,
                mid: sections.len().to_string(),
                direction: "recvonly".into(),
                payloads: parts.map(str::to_owned).collect(),
            });
        } else if let Some(section) = current.as_mut() {
            if let Some(mid) = line.strip_prefix("a=mid:") {
                section.mid = mid.to_owned();
            } else if matches!(line, "a=recvonly" | "a=sendonly" | "a=sendrecv" | "a=inactive") {
                section.direction = line.trim_start_matches("a=").to_owned();
            }
        }
    }
    if let Some(section) = current {
        sections.push(section);
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "\
v=0\r
o=- 0 0 IN IP4 127.0.0.1\r
s=-\r
t=0 0\r
m=audio 9 UDP/TLS/RTP/SAVPF 111\r
a=mid:0\r
a=recvonly\r
a=rtpmap:111 opus/48000/2\r
a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r
a=ice-ufrag:local\r
a=ice-pwd:localpwd\r
a=fingerprint:sha-256 AA:BB\r
m=video 9 UDP/TLS/RTP/SAVPF 102 103\r
a=mid:1\r
a=recvonly\r
a=rtpmap:102 H264/90000\r
a=fmtp:103 apt=102\r
a=rtpmap:103 rtx/90000\r
a=rtpmap:96 VP8/90000\r
";

    #[test]
    fn fragment_keeps_ice_and_opus() {
        let fragment = select_protocol_fragment(OFFER, None);
        assert!(fragment.contains("a=ice-ufrag:local"));
        assert!(fragment.contains("a=rtpmap:111 opus/48000/2"));
        assert!(fragment.contains("a=rtpmap:96 VP8/90000"));
        assert!(!fragment.contains("a=rtpmap:102 H264/90000"));
        assert!(!fragment.contains("m="));
    }

    #[test]
    fn answer_preserves_mline_count() {
        let server = "\
a=ice-ufrag:remote\r
a=ice-pwd:remotepwd\r
a=fingerprint:sha-256 CC:DD\r
a=candidate:1 1 udp 1 127.0.0.1 50000 typ host\r
c=IN IP4 127.0.0.1\r
";
        let answer = synthesize_answer(OFFER, server, &[42]).unwrap();
        assert_eq!(answer.matches("m=").count(), 2);
        assert!(answer.contains("a=ssrc:42 cname:discord-omt"));
        assert!(answer.contains("a=setup:passive"));
        assert!(answer.contains("a=ice-ufrag:remote"));
    }
}
