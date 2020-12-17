use crate::common::jcli::command::VotesCommand;
use assert_cmd::assert::OutputAssertExt;
use assert_fs::{assert::PathAssert, NamedTempFile};

pub mod committee;
mod crs;
mod tally;

pub use committee::Committee;
pub use crs::Crs;
pub use tally::Tally;

pub struct Votes {
    votes_command: VotesCommand,
}

impl Votes {
    pub fn new(votes_command: VotesCommand) -> Self {
        Self { votes_command }
    }

    pub fn committee(self) -> Committee {
        Committee::new(self.votes_command.committee())
    }

    pub fn crs(self) -> Crs {
        Crs::new(self.votes_command.crs())
    }

    pub fn encrypting_vote_key<S: Into<String>>(self, member_key: S) -> String {
        let output_file = NamedTempFile::new("encrypted_vote_key.tmp").unwrap();
        self.votes_command
            .encrypting_vote_key(member_key, output_file.path())
            .build()
            .assert()
            .success();

        output_file.assert(crate::predicate::file_exists_and_not_empty());
        jortestkit::prelude::read_file(output_file.path())
    }

    pub fn tally(self) -> Tally {
        Tally::new(self.votes_command.tally())
    }
}
