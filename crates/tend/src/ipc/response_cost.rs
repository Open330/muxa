//! Body-relative costs of the projected response, without copying body text.
use super::Response;
use crate::collaboration::{CollaborationReply, CollaborationRequest};

#[derive(Debug, Default)]
pub(super) struct BodyBytes {
    pub raw: usize,
    pub json: usize,
    pub unique: usize,
    seen: std::collections::HashSet<String>,
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
        let before = self.raw;
        let first = self.seen.insert(request.id.clone());
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
        if first {
            self.unique += self.raw - before;
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

    #[tokio::test]
    async fn copied_request_records_are_extra_over_the_single_copy_body_baseline() {
        use crate::collaboration::{
            CollaborationOptions, CollaborationStore, NewRequest, Participant, RoomId,
        };
        let console = Participant::console(RoomId {
            host: "tmux".into(),
            socket: Some("default".into()),
            window_id: "@1".into(),
        });
        let mut recipient = console.clone();
        recipient.console = false;
        recipient.agent_session_id = "peer".into();
        let store = CollaborationStore::in_memory(CollaborationOptions::default());
        let request = store
            .create(
                console,
                recipient,
                NewRequest {
                    body: "한글\\\"\n".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let mut response = Response::with_collaboration_request(request.clone());
        response.collaboration_requests = Some(vec![request.clone()]);
        let bytes = measure(&response);
        assert_eq!(bytes.raw, request.body.len() * 2);
        assert_eq!(bytes.unique, request.body.len());
        assert_eq!(
            bytes.json,
            (serde_json::to_vec(&request.body).unwrap().len() - 2) * 2
        );
        let mut projected = request;
        projected.omit_list_bodies();
        let metadata = measure(&Response::with_collaboration_request(projected));
        assert_eq!(metadata.raw, 0);
        assert_eq!(metadata.unique, 0);
    }
}
