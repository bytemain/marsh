use std::{
    path::{Path, PathBuf},
    sync::mpsc,
};

use crate::message::Message;
pub type CollectorTuple = (PathBuf, Vec<String>);
pub type CollectorSender = mpsc::Sender<Option<CollectorTuple>>;
pub type CollectorReceiver = mpsc::Receiver<Option<CollectorTuple>>;

pub struct CollectorService {
    pub deps: Vec<(String, String)>,

    sender: CollectorSender,
    receiver: CollectorReceiver,
}

impl Default for CollectorService {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            sender,
            receiver,
            deps: Vec::new(),
        }
    }
}

impl CollectorService {
    pub fn sender(&self) -> &CollectorSender {
        &self.sender
    }
    pub fn start(&mut self) {
        while let Ok(Some((path, deps))) = self.receiver.recv() {
            for dep in deps {
                self.deps.push((path.display().to_string(), dep));
            }
        }
    }

    pub fn wrap_messages(path: &Path, messages: Vec<Message>) -> CollectorTuple {
        let mut diagnostics = Vec::new();
        for message in messages {
            diagnostics.push(message.file_path);
        }
        (path.to_path_buf(), diagnostics)
    }
}
