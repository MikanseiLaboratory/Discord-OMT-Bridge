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

/// Opus, AV1, and H.264. AV1 is what the official client sends for camera and
/// Go Live, so it has to be marked decodable or the SFU forwards padding only.
pub fn advertisements_from_offer(local_sdp: &str) -> Vec<CodecAdvertisement> {
    let opus = payload_of(local_sdp, "opus").filter(|payload| *payload == 120).unwrap_or(120);
    vec![
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
            name: "AV1",
            kind: "video",
            priority: 3000,
            payload_type: 101,
            rtx_payload_type: Some(102),
            encode: false,
            decode: true,
        },
        CodecAdvertisement {
            name: "H264",
            kind: "video",
            priority: 1000,
            payload_type: 103,
            rtx_payload_type: Some(104),
            encode: false,
            decode: true,
        },
    ]
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

#[derive(Debug, Clone)]
pub struct TransportParams {
    pub ice_ufrag: String,
    pub ice_pwd: String,
    pub fingerprint: String,
    pub candidates: Vec<String>,
    pub connection: String,
    pub media_port: u16,
}

pub fn transport_from_server_sdp(sdp: &str) -> Option<TransportParams> {
    let mut ice_ufrag = None;
    let mut ice_pwd = None;
    let mut fingerprint = None;
    let mut candidates = Vec::new();
    let mut connection = None;
    let mut media_port = 0u16;
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("a=ice-ufrag:") {
            ice_ufrag = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("a=ice-pwd:") {
            ice_pwd = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("a=fingerprint:") {
            fingerprint = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("a=candidate:") {
            candidates.push(normalize_candidate(value));
        } else if line.starts_with("c=") && connection.is_none() {
            connection = Some(line.to_owned());
        } else if media_port == 0 {
            if let Some(rest) = line.strip_prefix("m=") {
                media_port = rest.split_whitespace().nth(1).and_then(|port| port.parse().ok()).unwrap_or(0);
            }
        }
    }
    let connection = connection.unwrap_or_else(|| "c=IN IP4 0.0.0.0".into());
    if candidates.is_empty() {
        if let Some(address) = connection.strip_prefix("c=IN IP4 ").map(str::trim) {
            if !address.is_empty() && address != "0.0.0.0" && media_port > 9 {
                candidates.push(format!("candidate:1 1 udp 2130706431 {address} {media_port} typ host"));
            }
        }
    }
    Some(TransportParams {
        ice_ufrag: ice_ufrag?,
        ice_pwd: ice_pwd?,
        fingerprint: fingerprint?,
        candidates,
        connection,
        media_port,
    })
}

fn normalize_candidate(value: &str) -> String {
    let mut parts: Vec<String> = value.split_whitespace().map(str::to_owned).collect();
    if parts.len() >= 3 {
        parts[2] = parts[2].to_ascii_lowercase();
    }
    format!("candidate:{}", parts.join(" "))
}

#[derive(Debug, Clone)]
struct OfferedSection {
    kind: String,
    mid: String,
    direction: String,
    payloads: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssignedSsrc {
    pub user_id: u64,
    pub ssrc: u32,
    pub rtx: u32,
}

/// Build an answer with the same m-line count as the local offer.
pub fn synthesize_answer(
    local_offer: &str,
    server_sdp: &str,
    audio: &[AssignedSsrc],
    video: &[AssignedSsrc],
) -> Option<String> {
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
    let mut audio_index = 0usize;
    let mut video_index = 0usize;
    for section in &sections {
        let payloads = if section.payloads.is_empty() {
            "0".to_owned()
        } else {
            section.payloads.join(" ")
        };
        let port = if transport.media_port > 9 { transport.media_port } else { 9 };
        out.push_str(&format!("m={} {port} UDP/TLS/RTP/SAVPF {payloads}\r\n", section.kind));
        out.push_str(&format!("{}\r\n", transport.connection));
        out.push_str("a=rtcp:9 IN IP4 0.0.0.0\r\n");
        out.push_str("a=setup:passive\r\n");
        out.push_str(&format!("a=mid:{}\r\n", section.mid));
        for candidate in &transport.candidates {
            out.push_str(&format!("a={candidate}\r\n"));
        }
        out.push_str("a=rtcp-mux\r\n");
        let first_audio = section.kind == "audio" && audio_index == 0;
        let assigned = if section.kind == "audio" {
            audio_index += 1;
            None
        } else if section.kind == "video" {
            let stream = video.get(video_index).copied();
            video_index += 1;
            stream
        } else {
            None
        };
        // The first audio m-line stays sendonly even before an SSRC is known.
        // inactive tells the SFU not to forward Opus at all.
        let direction = if first_audio {
            "sendonly"
        } else if section.kind == "audio" {
            "inactive"
        } else {
            answer_direction(&section.direction, assigned.is_some())
        };
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
        if first_audio {
            for stream in audio {
                let label = format!("a{}-{}", stream.user_id, stream.ssrc);
                out.push_str(&format!("a=msid:{label} {label}\r\n"));
                out.push_str(&format!("a=ssrc:{} cname:{label}\r\n", stream.ssrc));
            }
        }
        if let Some(stream) = assigned {
            let label = format!("v{}-{}", stream.user_id, stream.ssrc);
            out.push_str(&format!("a=msid:{label} {label}\r\n"));
            if stream.rtx != 0 {
                out.push_str(&format!("a=ssrc-group:FID {} {}\r\n", stream.ssrc, stream.rtx));
            }
            out.push_str(&format!("a=ssrc:{} cname:{label}\r\n", stream.ssrc));
            if stream.rtx != 0 {
                out.push_str(&format!("a=ssrc:{} cname:{label}\r\n", stream.rtx));
            }
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
        let answer = synthesize_answer(
            OFFER,
            server,
            &[
                AssignedSsrc { user_id: 1, ssrc: 11, rtx: 0 },
                AssignedSsrc { user_id: 2, ssrc: 22, rtx: 0 },
            ],
            &[AssignedSsrc { user_id: 7, ssrc: 42, rtx: 43 }],
        )
        .unwrap();
        assert_eq!(answer.matches("m=").count(), 2);
        assert!(answer.contains("a=ssrc:42 cname:v7-42"));
        assert!(answer.contains("a=ssrc-group:FID 42 43"));
        let audio = answer.split("m=video").next().unwrap_or("");
        assert!(audio.contains("a=sendonly"));
        assert!(audio.contains("a=ssrc:11 cname:a1-11"));
        assert!(audio.contains("a=ssrc:22 cname:a2-22"));
        assert!(!audio.contains("a=inactive"));
        let video = answer.split("m=video").nth(1).unwrap_or("");
        assert!(video.contains("a=sendonly"));
        assert!(answer.contains("a=setup:passive"));
        assert!(answer.contains("a=ice-ufrag:remote"));
        let media = answer.split_once("m=").map(|(_, rest)| rest).unwrap_or("");
        assert!(media.contains("a=candidate:1 1 udp 1 127.0.0.1 50000 typ host"));
    }

    #[test]
    fn connection_line_becomes_a_host_candidate() {
        let server = "\
m=audio 50010 UDP/TLS/RTP/SAVPF 111\r
c=IN IP4 203.0.113.8\r
a=ice-ufrag:remote\r
a=ice-pwd:remotepwd\r
a=fingerprint:sha-256 CC:DD\r
";
        let transport = transport_from_server_sdp(server).unwrap();
        assert_eq!(transport.candidates.len(), 1);
        assert!(transport.candidates[0].contains("203.0.113.8 50010"));
        let answer = synthesize_answer(OFFER, server, &[], &[]).unwrap();
        let media = answer.split_once("m=").unwrap().1;
        assert!(media.contains("a=candidate:1 1 udp 2130706431 203.0.113.8 50010 typ host"));
    }
}
