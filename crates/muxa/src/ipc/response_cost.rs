//! Body-relative costs of the projected response, without copying body text.
use super::Response;
use crate::collaboration::{CollaborationReply, CollaborationRequest};

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct BodyBytes {
    pub raw: usize,
    pub json: usize,
}

impl BodyBytes {
    fn text(&mut self, text: &str) {
        self.raw += text.len();
        // serde_json's escaped string contents, excluding the two quotes.
        self.json += text
            .bytes()
            .map(|b| match b {
                b'"' | b'\\' | b'\x08' | b'\t' | b'\n' | b'\x0c' | b'\r' => 2,
                0..=31 => 6,
                _ => 1,
            })
            .sum::<usize>();
    }

    fn reply(&mut self, reply: &CollaborationReply) {
        self.text(&reply.body);
    }

    fn request(&mut self, request: &CollaborationRequest) {
        self.text(&request.body);
        if let Some(reply) = &request.reply {
            self.reply(reply);
        }
        for reply in &request.late_replies {
            self.reply(reply);
        }
        for update in &request.updates {
            self.text(&update.body);
        }
        if let Some(reply) = request
            .interruption
            .as_ref()
            .and_then(|i| i.decision.as_ref())
        {
            self.reply(reply);
        }
    }
}

pub(super) fn measure(response: &Response) -> BodyBytes {
    let mut bytes = BodyBytes::default();
    for request in response
        .collaboration_request
        .iter()
        .chain(response.collaboration_requests.iter().flatten())
        .chain(
            response
                .collaboration_page
                .iter()
                .flat_map(|page| &page.requests),
        )
    {
        bytes.request(request);
    }
    if let Some(snapshot) = &response.collaboration_mailbox_snapshot {
        for request in snapshot.incoming.iter().chain(&snapshot.sent) {
            bytes.request(request);
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_accounting_matches_json_utf8_and_control_escaping_without_quotes() {
        let text = "한글😀\\\"\n\t\r\x08\x0c\0\x1f/<>";
        let mut bytes = BodyBytes::default();
        bytes.text(text);
        assert_eq!(bytes.raw, text.len());
        assert_eq!(bytes.json, serde_json::to_vec(text).unwrap().len() - 2);
    }
}
