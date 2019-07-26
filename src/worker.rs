use std;
use std::time::{Duration, Instant};

use actix::prelude::*;
use nix::unistd::Pid;

use config::ServiceConfig;
use event::{Events, Reason, State};
use process::{self, Process, ProcessError};
use service::FeService;
use utils::str;

#[allow(non_camel_case_types)]
#[derive(Serialize, Deserialize, PartialEq, Clone, Debug)]
#[serde(tag = "cmd", content = "data")]
pub enum WorkerCommand {
    prepare,
    start,
    pause,
    resume,
    stop,
    /// master heartbeat
    hb,
}

#[allow(non_camel_case_types)]
#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(tag = "cmd", content = "data")]
pub enum WorkerMessage {
    /// ready to execute worker in forked process
    forked,
    /// worker loaded
    loaded,
    /// worker requests reload
    reload,
    /// worker requests restart
    restart,
    /// worker configuration error
    cfgerror(String),
    /// heartbeat
    hb,
}

enum WorkerState {
    Initial,
    Starting(ProcessInfo),
    Reloading(ProcessInfo, ProcessInfo),
    Restarting(ProcessInfo, ProcessInfo),
    Running(ProcessInfo),
    StoppingOld(ProcessInfo, ProcessInfo),
    Stopping(ProcessInfo),
    Failed,
    Stopped,
}

struct ProcessInfo {
    pid: Pid,
    addr: Option<Addr<Process>>,
}

impl ProcessInfo {
    fn stop(&self) {
        if let Some(ref addr) = self.addr {
            addr.do_send(process::StopProcess);
        }
    }
    fn quit(&self, graceful: bool) {
        if let Some(ref addr) = self.addr {
            addr.do_send(process::QuitProcess(graceful));
        }
    }
    fn start(&self) {
        if let Some(ref addr) = self.addr {
            addr.do_send(process::StartProcess);
        }
    }
    fn pause(&self) {
        if let Some(ref addr) = self.addr {
            addr.do_send(process::PauseProcess);
        }
    }
    fn resume(&self) {
        if let Some(ref addr) = self.addr {
            addr.do_send(process::ResumeProcess);
        }
    }
}

pub struct Worker {
    pub idx: usize,
    cfg: ServiceConfig,
    state: WorkerState,
    pub events: Events,
    pub restore_from_fail: bool,
    started: Instant,
    restarts: u16,
    addr: Addr<FeService>,
}

impl Worker {
    pub fn new(idx: usize, cfg: ServiceConfig, addr: Addr<FeService>) -> Worker {
        Worker {
            idx,
            cfg,
            addr,
            state: WorkerState::Initial,
            events: Events::new(50),
            started: Instant::now(),
            restore_from_fail: false,
            restarts: 0,
        }
    }

    pub fn start(&mut self, reason: Reason) {
        let id = self.idx;
        match self.state {
            WorkerState::Initial | WorkerState::Stopped | WorkerState::Failed => {
                debug!("Starting worker process id: {:?}", id);
                let (pid, addr) = Process::start(self.idx, &self.cfg, self.addr.clone());
                self.state = WorkerState::Starting(ProcessInfo { pid, addr });
                self.events.add(State::Starting, reason, str(pid));
            }
            _ => (),
        }
    }

    pub fn loaded(&mut self, pid: Pid) {
        let state = std::mem::replace(&mut self.state, WorkerState::Initial);

        match state {
            WorkerState::Starting(p) => {
                if p.pid == pid {
                    self.restarts = 0;
                    p.start();
                    self.events.add(State::Running, Reason::None, str(p.pid));
                    self.state = WorkerState::Running(p);
                    self.restore_from_fail = false;
                } else {
                    self.state = WorkerState::Starting(p);
                }
            }
            WorkerState::Reloading(p, old) => {
                if p.pid == pid {
                    self.restarts = 0;
                    old.stop();
                    p.start();
                    self.events
                        .add(State::StoppingOld, Reason::None, str(old.pid));
                    self.state = WorkerState::StoppingOld(p, old);
                } else {
                    self.state = WorkerState::Reloading(p, old);
                }
            }
            WorkerState::Restarting(p, old) => {
                if p.pid == pid {
                    self.restarts = 0;
                    old.quit(true);
                    p.start();
                    self.events
                        .add(State::StoppingOld, Reason::None, str(old.pid));
                    self.state = WorkerState::StoppingOld(p, old);
                } else {
                    self.state = WorkerState::Restarting(p, old);
                }
            }
            state => self.state = state,
        };
    }

    pub fn is_running(&self) -> bool {
        match self.state {
            WorkerState::Running(_) => true,
            _ => false,
        }
    }

    pub fn is_failed(&self) -> bool {
        match self.state {
            WorkerState::Failed => true,
            WorkerState::Running(_) => self.restore_from_fail,
            _ => false,
        }
    }

    pub fn is_stopped(&self) -> bool {
        match self.state {
            WorkerState::Stopped => true,
            _ => false,
        }
    }

    pub fn pid(&self) -> Option<Pid> {
        match self.state {
            WorkerState::Running(ref process) => Some(process.pid),
            WorkerState::StoppingOld(ref process, _) => Some(process.pid),
            _ => None,
        }
    }

    pub fn reload(&mut self, graceful: bool, reason: Reason) {
        let state = std::mem::replace(&mut self.state, WorkerState::Initial);

        match state {
            WorkerState::Running(process) => {
                // start new worker
                let (pid, addr) = Process::start(self.idx, &self.cfg, self.addr.clone());
                let info = ProcessInfo { pid, addr };

                if graceful {
                    info!("Reloading worker: (pid:{})", process.pid);
                    self.events.add(State::Reloading, reason, str(process.pid));
                    self.state = WorkerState::Reloading(info, process);
                } else {
                    info!("Restarting worker: (pid:{})", process.pid);
                    self.events.add(State::Restarting, reason, str(process.pid));
                    self.state = WorkerState::Restarting(info, process);
                }
            }
            WorkerState::Failed | WorkerState::Stopped => {
                self.restarts = 0;
                self.state = WorkerState::Initial;
                self.start(reason);
            }
            _ => self.state = state,
        }
    }

    pub fn stop(&mut self, reason: Reason) {
        let state = std::mem::replace(&mut self.state, WorkerState::Initial);

        match state {
            WorkerState::Initial | WorkerState::Stopped | WorkerState::Failed => {
                self.state = WorkerState::Stopped;
                self.events.add(State::Stopped, reason, None);
            }
            WorkerState::Starting(process) => {
                process.quit(true);
                self.events.add(State::Stopping, reason, str(process.pid));
                self.state = WorkerState::Stopping(process);
            }
            WorkerState::Stopping(process) => {
                self.state = WorkerState::Stopping(process)
            }
            WorkerState::StoppingOld(process, old_proc) => {
                old_proc.quit(true);
                process.stop();
                self.events.add(State::Stopping, reason, str(process.pid));
                self.state = WorkerState::Stopping(process);
            }
            WorkerState::Running(process) => {
                process.stop();
                self.events.add(State::Stopping, reason, str(process.pid));
                self.state = WorkerState::Stopping(process);
            }
            WorkerState::Reloading(process, old_proc) => {
                process.quit(true);
                old_proc.stop();
                self.events.add(State::Stopping, reason, str(old_proc.pid));
                self.state = WorkerState::Stopping(old_proc);
            }
            WorkerState::Restarting(process, old_proc) => {
                process.quit(true);
                old_proc.stop();
                self.events.add(State::Stopping, reason, str(old_proc.pid));
                self.state = WorkerState::Stopping(old_proc);
            }
        }
    }

    pub fn quit(&mut self, reason: Reason) {
        let state = std::mem::replace(&mut self.state, WorkerState::Initial);

        match state {
            WorkerState::Initial | WorkerState::Stopped | WorkerState::Failed => {
                self.state = WorkerState::Stopped;
                self.events.add(State::Stopped, reason, None);
            }
            WorkerState::Starting(process) => {
                process.quit(true);
                self.events.add(State::Stopping, reason, str(process.pid));
                self.state = WorkerState::Stopping(process);
            }
            WorkerState::Stopping(process) => {
                self.state = WorkerState::Stopping(process)
            }
            WorkerState::StoppingOld(process, old_proc) => {
                old_proc.quit(true);
                process.quit(true);
                self.events
                    .add(State::StoppingOld, reason, str(process.pid));
                self.state = WorkerState::Stopping(process);
            }
            WorkerState::Running(process) => {
                process.quit(true);
                self.events.add(State::Stopping, reason, str(process.pid));
                self.state = WorkerState::Stopping(process);
            }
            WorkerState::Reloading(process, old_proc) => {
                process.quit(true);
                old_proc.quit(true);
                self.events.add(State::Stopping, reason, str(old_proc.pid));
                self.state = WorkerState::Stopping(old_proc);
            }
            WorkerState::Restarting(process, old_proc) => {
                process.quit(true);
                old_proc.quit(true);
                self.events.add(State::Stopping, reason, str(old_proc.pid));
                self.state = WorkerState::Stopping(old_proc);
            }
        }
    }

    pub fn message(&mut self, pid: Pid, message: &WorkerMessage) {
        let reload = match self.state {
            WorkerState::Running(ref process) => process.pid == pid,
            _ => false,
        };

        if reload {
            match *message {
                WorkerMessage::reload => self.reload(true, Reason::WorkerRequest),
                WorkerMessage::restart => self.reload(false, Reason::WorkerRequest),
                _ => (),
            }
        }
    }

    pub fn pause(&mut self, reason: Reason) {
        if let WorkerState::Running(ref process) = self.state {
            process.pause();
            self.events.add(State::Paused, reason, str(process.pid));
        }
    }

    pub fn resume(&mut self, reason: Reason) {
        if let WorkerState::Running(ref process) = self.state {
            process.resume();
            self.events.add(State::Running, reason, str(process.pid));
        }
    }

    pub fn exited(&mut self, pid: Pid, err: &ProcessError) {
        let state = std::mem::replace(&mut self.state, WorkerState::Initial);

        match state {
            WorkerState::Running(process) => {
                if process.pid != pid {
                    self.state = WorkerState::Running(process);
                } else {
                    match *err {
                        ProcessError::StartupTimeout => {
                            self.state = WorkerState::Running(process);
                            self.events.add(State::Running, err.into(), str(pid));
                            self.restore_from_fail = true;
                            self.reload(false, Reason::ReloadAftreTimeout);
                            return;
                        }
                        _ => {
                            // kill worker
                            process.quit(false);

                            // start new worker
                            self.started = Instant::now();
                            self.state = WorkerState::Initial;
                            self.events.add(State::Stopped, err.into(), str(pid));
                            self.start(Reason::RestartFailedRunningWorker);
                        }
                    }
                }
            }
            WorkerState::Starting(process) => {
                // new process died, need to restart
                if process.pid != pid {
                    self.state = WorkerState::Starting(process);
                } else {
                    match *err {
                        // can not boot worker, fail immediately
                        //&ProcessError::InitFailed | &ProcessError::BootFailed => {
                        //    self.state = WorkerState::Failed;
                        //    self.events.add(State::Failed, Reason::from(err), str(pid));
                        //    return
                        //}
                        ProcessError::ExitCode(0) => {
                            // check for fast restart
                            let now = Instant::now();
                            if now.duration_since(self.started) > Duration::new(10, 0) {
                                self.started = now;
                                self.restarts = 0;
                            } else {
                                self.restarts += 1;
                            }
                        }
                        _ => self.restarts += 1,
                    }

                    self.events.add(State::Failed, Reason::from(err), str(pid));

                    if self.restarts < self.cfg.restarts {
                        // just in case
                        process.quit(false);

                        // start new worker
                        self.state = WorkerState::Initial;
                        self.start(Reason::RestartFailedStartingWorker);
                    } else {
                        error!("Can not start worker (pid:{})", process.pid);
                        self.state = WorkerState::Failed;
                    }
                }
            }
            WorkerState::Reloading(process, old_proc) => {
                // new process died, need to restart
                if process.pid == pid {
                    // can not boot worker, restore old process
                    match *err {
                        //&ProcessError::InitFailed | &ProcessError::BootFailed => {
                        //    error!("Can not start worker (pid:{}), restoring old worker",
                        //           process.pid);
                        //    self.restore_from_fail = true;
                        //    self.events.add(State::ReloadFailed, err.into(), str(pid));
                        //    self.events.add(State::Running,
                        //                    Reason::RestoreAfterFailed, str(old_proc.pid));
                        //    self.state = WorkerState::Running(old_proc);
                        //    return
                        //}
                        ProcessError::ExitCode(0) => {
                            // check for fast restart
                            let now = Instant::now();
                            if now.duration_since(self.started) > Duration::new(3, 0) {
                                self.started = now;
                                self.restarts = 0;
                            } else {
                                self.restarts += 1;
                            }
                        }
                        _ => self.restarts += 1,
                    }

                    self.events.add(State::ReloadFailed, err.into(), str(pid));

                    if self.restarts < self.cfg.restarts {
                        // start new worker
                        let (pid, addr) =
                            Process::start(self.idx, &self.cfg, self.addr.clone());
                        let info = ProcessInfo { pid, addr };
                        self.state = WorkerState::Reloading(info, old_proc);
                    } else {
                        error!(
                            "Can not start worker (pid:{}), restoring old worker",
                            process.pid
                        );
                        self.restore_from_fail = true;
                        self.events.add(
                            State::Running,
                            Reason::RestoreAftreFailed,
                            str(old_proc.pid),
                        );
                        self.state = WorkerState::Running(old_proc);
                    }
                } else if old_proc.pid == pid {
                    self.restore_from_fail = false;
                    self.events.add(State::Stopped, Reason::None, str(pid));
                    self.events
                        .add(State::Running, Reason::None, str(process.pid));
                    self.state = WorkerState::Running(process);
                } else {
                    self.state = WorkerState::Reloading(process, old_proc);
                }
            }
            WorkerState::Restarting(process, old_proc) => {
                // new process died, need to restart
                if process.pid == pid {
                    // can not boot worker, restore old process
                    match *err {
                        //&ProcessError::InitFailed | &ProcessError::BootFailed => {
                        //    error!("Can not start worker (pid:{}), restoring old worker",
                        //           process.pid);
                        //    self.restore_from_fail = true;
                        //    self.events.add(State::RestartFailed, err.into(), str(pid));
                        //    self.events.add(State::Running,
                        //                    Reason::RestoreAfterFailed, str(old_proc.pid));
                        //    self.state = WorkerState::Running(old_proc);
                        //    return
                        //},
                        ProcessError::ExitCode(0) => {
                            // check for fast restart
                            let now = Instant::now();
                            if now.duration_since(self.started) > Duration::new(3, 0) {
                                self.started = now;
                                self.restarts = 0;
                            } else {
                                self.restarts += 1;
                            }
                        }
                        _ => {
                            self.restarts += 1;
                        }
                    }

                    self.events.add(State::RestartFailed, err.into(), str(pid));

                    if self.restarts < self.cfg.restarts {
                        // start new worker
                        let (pid, addr) =
                            Process::start(self.idx, &self.cfg, self.addr.clone());
                        let info = ProcessInfo { pid, addr };
                        self.state = WorkerState::Restarting(info, old_proc);
                    } else {
                        error!(
                            "Can not start worker (pid:{}), restoring old worker",
                            process.pid
                        );
                        self.restore_from_fail = true;
                        self.events.add(
                            State::Running,
                            Reason::RestoreAftreFailed,
                            str(old_proc.pid),
                        );
                        self.state = WorkerState::Running(old_proc);
                    }
                } else if old_proc.pid == pid {
                    self.restore_from_fail = false;
                    self.events.add(State::Stopped, Reason::None, str(pid));
                    self.events
                        .add(State::Running, Reason::None, str(process.pid));
                    self.state = WorkerState::Running(process);
                } else {
                    self.state = WorkerState::Restarting(process, old_proc);
                }
            }
            WorkerState::StoppingOld(process, old_proc) => {
                // new process died, need to restart
                if process.pid == pid {
                    old_proc.quit(false);
                    self.restarts += 1;
                    self.state = WorkerState::Initial;
                    self.events.add(State::Failed, err.into(), str(pid));
                    self.start(Reason::NewProcessDied);
                } else if old_proc.pid == pid {
                    self.restore_from_fail = false;
                    self.events.add(State::Stopped, Reason::None, str(pid));
                    self.events
                        .add(State::Running, Reason::None, str(process.pid));
                    self.state = WorkerState::Running(process);
                } else {
                    self.state = WorkerState::StoppingOld(process, old_proc);
                }
            }
            WorkerState::Stopping(process) => {
                if process.pid == pid {
                    self.state = WorkerState::Stopped;
                    self.events.add(State::Stopped, err.into(), str(pid));
                } else {
                    self.state = WorkerState::Stopping(process);
                }
            }
            state => self.state = state,
        }
    }
}
