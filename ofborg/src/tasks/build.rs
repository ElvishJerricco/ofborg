extern crate amqp;
extern crate uuid;
extern crate env_logger;

use uuid::Uuid;

use std::collections::VecDeque;
use ofborg::checkout;
use ofborg::message::buildjob;
use ofborg::message::buildresult::{BuildResult, BuildStatus, V1Tag};
use ofborg::message::buildlogmsg;
use ofborg::nix;
use ofborg::commentparser;

use ofborg::worker;
use ofborg::notifyworker;
use amqp::protocol::basic::{Deliver, BasicProperties};


pub struct BuildWorker {
    cloner: checkout::CachedCloner,
    nix: nix::Nix,
    system: String,
    identity: String,
}

impl BuildWorker {
    pub fn new(
        cloner: checkout::CachedCloner,
        nix: nix::Nix,
        system: String,
        identity: String,
    ) -> BuildWorker {
        return BuildWorker {
            cloner: cloner,
            nix: nix,
            system: system,
            identity: identity,
        };
    }

    fn actions<'a, 'b>(
        &self,
        job: &'b buildjob::BuildJob,
        receiver: &'a mut notifyworker::NotificationReceiver,
    ) -> JobActions<'a, 'b> {
        JobActions::new(&self.system, &self.identity, job, receiver)
    }
}

pub struct JobActions<'a, 'b> {
    system: String,
    identity: String,
    receiver: &'a mut notifyworker::NotificationReceiver,
    job: &'b buildjob::BuildJob,
    line_counter: u64,
    snippet_log: VecDeque<String>,
    attempt_id: String,
    log_exchange: Option<String>,
    log_routing_key: Option<String>,
    result_exchange: Option<String>,
    result_routing_key: Option<String>,
}

impl<'a, 'b> JobActions<'a, 'b> {
    pub fn new(
        system: &str,
        identity: &str,
        job: &'b buildjob::BuildJob,
        receiver: &'a mut notifyworker::NotificationReceiver,
    ) -> JobActions<'a, 'b> {
        let (log_exchange, log_routing_key) = job.logs.clone().unwrap_or((
            Some(String::from("logs")),
            Some(String::from("build.log")),
        ));

        let (result_exchange, result_routing_key) =
            job.statusreport.clone().unwrap_or((
                Some(String::from("build-results")),
                None,
            ));

        return JobActions {
            system: system.to_owned(),
            identity: identity.to_owned(),
            receiver: receiver,
            job: job,
            line_counter: 0,
            snippet_log: VecDeque::with_capacity(10),
            attempt_id: format!("{}", Uuid::new_v4()),
            log_exchange: log_exchange,
            log_routing_key: log_routing_key,
            result_exchange: result_exchange,
            result_routing_key: result_routing_key,
        };
    }

    pub fn log_snippet(&self) -> Vec<String> {
        self.snippet_log.clone().into()
    }

    pub fn commit_missing(&mut self) {
        self.tell(worker::Action::Ack);
    }

    pub fn nasty_hack_linux_only(&mut self) {
        self.tell(worker::Action::Ack);
    }

    pub fn nothing_to_do(&mut self) {
        self.tell(worker::Action::Ack);
    }

    pub fn merge_failed(&mut self) {
        let msg = BuildResult::V1 {
            tag: V1Tag::V1,
            repo: self.job.repo.clone(),
            pr: self.job.pr.clone(),
            system: self.system.clone(),
            output: vec![String::from("Merge failed")],
            attempt_id: self.attempt_id.clone(),
            request_id: self.job.request_id.clone(),
            attempted_attrs: None,
            skipped_attrs: None,
            status: BuildStatus::Failure,
        };

        let result_exchange = self.result_exchange.clone();
        let result_routing_key = self.result_routing_key.clone();


        self.tell(worker::publish_serde_action(
            result_exchange,
            result_routing_key,
            &msg,
        ));
        self.tell(worker::Action::Ack);
    }

    pub fn log_started(&mut self, can_build: Vec<String>, cannot_build: Vec<String>) {
        let msg = buildlogmsg::BuildLogStart {
            identity: self.identity.clone(),
            system: self.system.clone(),
            attempt_id: self.attempt_id.clone(),
            attempted_attrs: Some(can_build),
            skipped_attrs: Some(cannot_build),
        };

        let log_exchange = self.log_exchange.clone();
        let log_routing_key = self.log_routing_key.clone();

        self.tell(worker::publish_serde_action(
            log_exchange,
            log_routing_key,
            &msg,
        ));
    }

    pub fn log_instantiation_errors(&mut self, cannot_build: Vec<(String, Vec<String>)>) {
        for (attr, log) in cannot_build {
            self.log_line(&format!("Cannot nix-instantiate `{}' because:", &attr));

            for line in log {
                self.log_line(&line);
            }
            self.log_line("");
        }
    }

    pub fn log_line(&mut self, line: &str) {
        self.line_counter += 1;

        if self.snippet_log.len() >= 10 {
            self.snippet_log.pop_front();
        }
        self.snippet_log.push_back(line.to_owned());

        let msg = buildlogmsg::BuildLogMsg {
            identity: self.identity.clone(),
            system: self.system.clone(),
            attempt_id: self.attempt_id.clone(),
            line_number: self.line_counter,
            output: line.to_owned(),
        };

        let log_exchange = self.log_exchange.clone();
        let log_routing_key = self.log_routing_key.clone();

        self.tell(worker::publish_serde_action(
            log_exchange,
            log_routing_key,
            &msg,
        ));
    }

    pub fn build_not_attempted(&mut self, not_attempted_attrs: Vec<String>,

    ) {
        let msg = BuildResult::V1 {
            tag: V1Tag::V1,
            repo: self.job.repo.clone(),
            pr: self.job.pr.clone(),
            system: self.system.clone(),
            output: self.log_snippet(),
            attempt_id: self.attempt_id.clone(),
            request_id: self.job.request_id.clone(),
            skipped_attrs: Some(not_attempted_attrs),
            attempted_attrs: None,
            status: BuildStatus::Skipped,
        };

        let result_exchange = self.result_exchange.clone();
        let result_routing_key = self.result_routing_key.clone();
        self.tell(worker::publish_serde_action(
            result_exchange,
            result_routing_key,
            &msg,
        ));

        let log_exchange = self.log_exchange.clone();
        let log_routing_key = self.log_routing_key.clone();
        self.tell(worker::publish_serde_action(
            log_exchange,
            log_routing_key,
            &msg,
        ));

        self.tell(worker::Action::Ack);
    }

    pub fn build_finished(&mut self, status: BuildStatus,
                          attempted_attrs: Vec<String>,
                          not_attempted_attrs: Vec<String>,

    ) {
        let msg = BuildResult::V1 {
            tag: V1Tag::V1,
            repo: self.job.repo.clone(),
            pr: self.job.pr.clone(),
            system: self.system.clone(),
            output: self.log_snippet(),
            attempt_id: self.attempt_id.clone(),
            request_id: self.job.request_id.clone(),
            status: status,
            attempted_attrs: Some(attempted_attrs),
            skipped_attrs: Some(not_attempted_attrs),
        };

        let result_exchange = self.result_exchange.clone();
        let result_routing_key = self.result_routing_key.clone();
        self.tell(worker::publish_serde_action(
            result_exchange,
            result_routing_key,
            &msg,
        ));

        let log_exchange = self.log_exchange.clone();
        let log_routing_key = self.log_routing_key.clone();
        self.tell(worker::publish_serde_action(
            log_exchange,
            log_routing_key,
            &msg,
        ));

        self.tell(worker::Action::Ack);
    }

    fn tell(&mut self, action: worker::Action) {
        self.receiver.tell(action);
    }
}

impl notifyworker::SimpleNotifyWorker for BuildWorker {
    type J = buildjob::BuildJob;

    fn msg_to_job(
        &self,
        _: &Deliver,
        _: &BasicProperties,
        body: &Vec<u8>,
    ) -> Result<Self::J, String> {
        println!("lmao I got a job?");
        return match buildjob::from(body) {
            Ok(e) => Ok(e),
            Err(e) => {
                println!("{:?}", String::from_utf8(body.clone()));
                panic!("{:?}", e);
            }
        };
    }

    fn consumer(
        &self,
        job: &buildjob::BuildJob,
        notifier: &mut notifyworker::NotificationReceiver,
    ) {
        let mut actions = self.actions(&job, notifier);

        if job.attrs.len() == 0 {
            actions.nothing_to_do();
            return;
        }

        info!("Working on {}", job.pr.number);
        let project = self.cloner.project(
            job.repo.full_name.clone(),
            job.repo.clone_url.clone(),
        );
        let co = project
            .clone_for("builder".to_string(), self.identity.clone())
            .unwrap();

        let target_branch = match job.pr.target_branch.clone() {
            Some(x) => x,
            None => String::from("origin/master"),
        };

        let buildfile = match job.subset {
            Some(commentparser::Subset::NixOS) => "./nixos/release.nix",
            _ => "./default.nix",
        };

        if buildfile == "./nixos/release.nix" && self.system == "x86_64-darwin" {
            actions.nasty_hack_linux_only();
            return;
        }

        let refpath = co.checkout_origin_ref(target_branch.as_ref()).unwrap();
        co.fetch_pr(job.pr.number).unwrap();

        if !co.commit_exists(job.pr.head_sha.as_ref()) {
            info!("Commit {} doesn't exist", job.pr.head_sha);
            actions.commit_missing();
            return;
        }

        if let Err(_) = co.merge_commit(job.pr.head_sha.as_ref()) {
            info!("Failed to merge {}", job.pr.head_sha);
            actions.merge_failed();
            return;
        }

        println!("Got path: {:?}, determining which ones we can build ", refpath);
        let (can_build, cannot_build) = self.nix.safely_partition_instantiable_attrs(
            refpath.as_ref(),
            buildfile,
            job.attrs.clone(),
        );

        let cannot_build_attrs: Vec<String> = cannot_build
            .clone()
            .into_iter()
            .map(|(attr,_)| attr)
            .collect();

        println!("Can build: '{}', Cannot build: '{}'",
                 can_build.join(", "),
                 cannot_build_attrs.join(", "));


        actions.log_started(can_build.clone(), cannot_build_attrs.clone());
        actions.log_instantiation_errors(cannot_build);

        if can_build.len() == 0 {
            actions.build_not_attempted(cannot_build_attrs);
            return;
        }

        let mut spawned = self.nix.safely_build_attrs_async(
            refpath.as_ref(),
            buildfile,
            can_build.clone(),
        );

        for line in spawned.lines() {
            actions.log_line(&line);
        }

        // TODO: this belongs in the nix module.
        let status = match spawned.wait() {
            Ok(s) => match s.code() {
                Some(0) => BuildStatus::Success,
                Some(100) => BuildStatus::Failure, // nix permanent failure
                Some(101) => BuildStatus::TimedOut, // nix build timedout
                Some(i) => BuildStatus::UnexpectedError {
                    err: format!("command failed with exit code {}", i),
                },
                None => BuildStatus::UnexpectedError {
                    err: "unexpected build failure".into(),
                },
            },
            e => BuildStatus::UnexpectedError {
                err: format!("failed on interior command {:?}", e),
            }
        };

        println!("ok built ({:?}), building", status);
        println!("Lines:\n-----8<-----");
        actions.log_snippet().iter().inspect(|x| println!("{}", x)).last();
        println!("----->8-----");

        actions.build_finished(status, can_build, cannot_build_attrs);
        println!("Done!");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::path::{Path, PathBuf};
    use ofborg::message::{Pr, Repo};
    use notifyworker::SimpleNotifyWorker;
    use std::process::{Command, Stdio};
    use std::vec::IntoIter;
    use ofborg::test_scratch::TestScratch;

    fn nix() -> nix::Nix {
        let remote = env::var("NIX_REMOTE").unwrap_or("".to_owned());
        nix::Nix::new("x86_64-linux".to_owned(), remote, 1800, None)
    }

    fn tpath(component: &str) -> PathBuf {
        return Path::new(env!("CARGO_MANIFEST_DIR")).join(component);
    }

    fn make_worker(path: &Path) -> BuildWorker {
        let cloner = checkout::cached_cloner(path);
        let nix = nix();
        let worker = BuildWorker::new(
            cloner,
            nix,
            "x86_64-linux".to_owned(),
            "cargo-test-build".to_owned(),
        );

        return worker;
    }

    fn make_pr_repo(bare: &Path, co: &Path) -> String {
        let output = Command::new("./make-pr.sh")
            .current_dir(tpath("./test-srcs"))
            .arg(bare)
            .arg(co)
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .output()
            .expect("building the test PR failed");
        let hash = String::from_utf8(output.stdout).expect("Should just be a hash");
        return hash.trim().to_owned();
    }

    fn strip_escaped_ansi(string: &str) -> String {
        string
            .replace("‘", "'")
            .replace("’", "'")
            .replace("\\u001b[31;1m", "") // red
            .replace("\\u001b[0m", "") // reset
    }

    fn assert_contains_job(actions: &mut IntoIter<worker::Action>, text_to_match: &str) {
        println!("\n\n   Searching: {:?}", text_to_match);
        actions
            .position(|job| match job {
                worker::Action::Publish(ref body) => {
                    let content = String::from_utf8(body.content.clone()).unwrap();
                    let text = strip_escaped_ansi(&content);
                    if text.contains(text_to_match) {
                        println!(" ok");
                        return true;
                    } else {
                        println!(" notContains: {:?}", text);
                        return false;
                    }
                }
                e => {
                    println!(" notPublish: {:?}", e);
                    return false;
                }
            })
            .expect(&format!(
                "Actions should contain a job matching {:?}, after the previous check",
                text_to_match
            ));
    }

    #[test]
    pub fn test_simple_build() {
        let p = TestScratch::new_dir("build-simple-build-working");
        let bare_repo = TestScratch::new_dir("build-simple-build-bare");
        let co_repo = TestScratch::new_dir("build-simple-build-co");

        let head_sha = make_pr_repo(&bare_repo.path(), &co_repo.path());
        let worker = make_worker(&p.path());

        let job = buildjob::BuildJob {
            attrs: vec!["success".to_owned()],
            pr: Pr {
                head_sha: head_sha,
                number: 1,
                target_branch: Some("master".to_owned()),
            },
            repo: Repo {
                clone_url: bare_repo.path().to_str().unwrap().to_owned(),
                full_name: "test-git".to_owned(),
                name: "nixos".to_owned(),
                owner: "ofborg-test".to_owned(),
            },
            subset: None,
            logs: Some((
                Some(String::from("logs")),
                Some(String::from("build.log")),
            )),
            statusreport: Some((Some(String::from("build-results")), None)),
            request_id: "bogus-request-id".to_owned(),
        };

        let mut dummyreceiver = notifyworker::DummyNotificationReceiver::new();

        worker.consumer(&job, &mut dummyreceiver);

        println!("Total actions: {:?}", dummyreceiver.actions.len());
        let mut actions = dummyreceiver.actions.into_iter();

        assert_contains_job(&mut actions, "output\":\"hi");
        assert_contains_job(&mut actions, "output\":\"1");
        assert_contains_job(&mut actions, "output\":\"2");
        assert_contains_job(&mut actions, "output\":\"3");
        assert_contains_job(&mut actions, "output\":\"4");
        assert_contains_job(&mut actions, "status\":\"Success\""); // First one to the github poster
        assert_contains_job(&mut actions, "status\":\"Success\""); // This one to the logs
        assert_eq!(actions.next(), Some(worker::Action::Ack));
    }


    #[test]
    pub fn test_all_jobs_skipped() {
        let p = TestScratch::new_dir("no-attempt");
        let bare_repo = TestScratch::new_dir("no-attempt-bare");
        let co_repo = TestScratch::new_dir("no-attempt-co");

        let head_sha = make_pr_repo(&bare_repo.path(), &co_repo.path());
        let worker = make_worker(&p.path());

        let job = buildjob::BuildJob {
            attrs: vec!["not-real".to_owned()],
            pr: Pr {
                head_sha: head_sha,
                number: 1,
                target_branch: Some("master".to_owned()),
            },
            repo: Repo {
                clone_url: bare_repo.path().to_str().unwrap().to_owned(),
                full_name: "test-git".to_owned(),
                name: "nixos".to_owned(),
                owner: "ofborg-test".to_owned(),
            },
            subset: None,
            logs: Some((
                Some(String::from("logs")),
                Some(String::from("build.log")),
            )),
            statusreport: Some((Some(String::from("build-results")), None)),
            request_id: "bogus-request-id".to_owned(),
        };

        let mut dummyreceiver = notifyworker::DummyNotificationReceiver::new();

        worker.consumer(&job, &mut dummyreceiver);

        println!("Total actions: {:?}", dummyreceiver.actions.len());
        let mut actions = dummyreceiver.actions.into_iter();
        assert_contains_job(&mut actions, "\"line_number\":1,\"output\":\"Cannot nix-instantiate `not-real\' because:\"");
        assert_contains_job(&mut actions, "\"line_number\":2,\"output\":\"error: attribute 'not-real' in selection path 'not-real' not found\"}");
        assert_contains_job(&mut actions, "skipped_attrs\":[\"not-real"); // First one to the github poster
        assert_contains_job(&mut actions, "skipped_attrs\":[\"not-real"); // This one to the logs
        assert_eq!(actions.next(), Some(worker::Action::Ack));
    }
}
