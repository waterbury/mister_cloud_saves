#!/usr/bin/env python

# Copyright (c) 2025-2026 bleach86

# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation, either version 3 of the License, or
# (at your option) any later version.

# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU General Public License for more details.

# You should have received a copy of the GNU General Public License
# along with this program.  If not, see <http://www.gnu.org/licenses/>.

# You can download the latest version of this tool from:
# https://github.com/bleach86/mister_cloud_saves

import datetime
import os
import signal
import subprocess
import sys
import shutil
import time


MISTER_PATH = "/media/fat"
CLIENT_DIR = os.path.join(MISTER_PATH, "cloud_saves")
UPDATES_DIR = os.path.join(CLIENT_DIR, "updates")
LAUNCHER_LOG_PATH = os.path.join(CLIENT_DIR, "launcher.log")
MAX_LOG_LINES = 500

# MiSTer has no systemd (or any other service manager) to restart a crashed
# process, so the supervisor below needs its own pid file, separate from the
# client's (/var/run/mister_save_client.pid, written by the client itself).
# cloud_saves.sh signals this one to stop the client *and* tell the
# supervisor not to bring it back - signaling the client's pid alone would
# just have the supervisor relaunch it immediately.
SUPERVISOR_PID_PATH = "/var/run/mister_save_client_launcher.pid"

# Set by _handle_stop_signal, read by supervise_client.
_stop_requested = False
# The client subprocess.Popen currently being supervised, if any, so a stop
# signal can be forwarded to it immediately instead of waiting for it to
# exit on its own.
_current_child = None


def check_updates():
    """
    Checks for updates to the Mister Cloud Saves Client.
    """
    update_file = os.path.join(UPDATES_DIR, "client.tar.xz")

    if os.path.isfile(update_file):
        extract_client(update_file)


def extract_client(update_file_path):
    """
    Extracts the client archive to the client directory.
    """
    if os.path.isfile(update_file_path):
        if not os.path.isdir(CLIENT_DIR):
            os.makedirs(CLIENT_DIR)

        shutil.unpack_archive(update_file_path, CLIENT_DIR, format="xztar")
        os.remove(update_file_path)
    else:
        print("Client archive not found")
        sys.exit(1)


def _log(message):
    """
    Appends a timestamped line to the supervisor's own small log. Once
    detached from user-startup.sh the supervisor has no terminal, so this is
    the only record of client starts, stops, crashes and restarts. Trimmed to
    the most recent lines so it can't grow without bound.
    """
    line = "{}Z {}".format(
        datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S"),
        message,
    )

    try:
        lines = []
        if os.path.isfile(LAUNCHER_LOG_PATH):
            with open(LAUNCHER_LOG_PATH, "r", encoding="utf-8") as f:
                lines = f.readlines()

        lines.append(line + "\n")
        if len(lines) > MAX_LOG_LINES:
            lines = lines[-MAX_LOG_LINES:]

        with open(LAUNCHER_LOG_PATH, "w", encoding="utf-8") as f:
            f.writelines(lines)
    except OSError:
        pass


def _handle_stop_signal(signum, _frame):
    """
    Forwards an intentional stop (sent by cloud_saves.sh before an update,
    server change, or uninstall) to the running client and tells the
    supervisor loop not to restart it. Without this, stopping the client for
    an update would just have the supervisor bring the old binary straight
    back up, possibly while the new one is being extracted on top of it.
    """
    global _stop_requested
    _stop_requested = True
    _log(f"received signal {signum}, stopping (will not restart)")

    if _current_child is not None and _current_child.poll() is None:
        try:
            _current_child.send_signal(signum)
        except OSError:
            pass


def _write_supervisor_pid():
    try:
        with open(SUPERVISOR_PID_PATH, "w", encoding="utf-8") as f:
            f.write(str(os.getpid()))
    except OSError as e:
        _log(f"failed to write supervisor pid file: {e}")


def _remove_supervisor_pid():
    try:
        os.remove(SUPERVISOR_PID_PATH)
    except OSError:
        pass


def _interruptible_sleep(seconds):
    """
    Sleeps up to `seconds`, but wakes early once a stop has been requested.

    A signal arriving during a plain time.sleep() does not cut it short:
    PEP 475 has Python automatically resume the same sleep for whatever time
    is left once the handler returns, rather than returning early. Without
    this, a stop request arriving during a long backoff delay would sit
    unnoticed until that delay ran out (up to max_backoff seconds) before
    the supervisor actually exited.
    """
    step = 0.2
    remaining = seconds
    while remaining > 0 and not _stop_requested:
        time.sleep(min(step, remaining))
        remaining -= step


def supervise_client(client_executable):
    """
    Keeps the client running for the life of the system.

    Previously, once started, the client was never watched again: if it hit
    an unhandled error and exited, saves simply stopped syncing until the
    next reboot, with nothing to say why. This restarts it with capped
    exponential backoff, resetting the backoff once a run has been stable
    for a while so a single rough patch doesn't leave restarts permanently
    slow.
    """
    global _current_child

    backoff = 2
    max_backoff = 60
    stable_after = 60  # seconds

    _log(f"supervisor started (pid {os.getpid()}), watching {client_executable}")

    while not _stop_requested:
        started = time.monotonic()

        try:
            _current_child = subprocess.Popen(
                [client_executable],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                stdin=subprocess.DEVNULL,
            )
        except OSError as e:
            _log(f"failed to start client: {e}")
            _interruptible_sleep(backoff)
            backoff = min(backoff * 2, max_backoff)
            continue

        _log(f"client started (pid {_current_child.pid})")
        exit_code = _current_child.wait()
        _current_child = None
        ran_for = time.monotonic() - started

        if _stop_requested:
            _log(
                f"client exited (code={exit_code}) after {ran_for:.1f}s, "
                "stop requested, not restarting"
            )
            break

        _log(f"client exited unexpectedly (code={exit_code}) after {ran_for:.1f}s")

        backoff = 2 if ran_for >= stable_after else min(backoff * 2, max_backoff)
        _log(f"restarting client in {backoff}s")
        _interruptible_sleep(backoff)

    _log("supervisor exiting")


def _daemonize_supervisor(client_executable):
    """
    Double-forks so the supervisor is fully detached from this short-lived
    process (the same way the client used to detach on its own via
    start_new_session), but keeps running indefinitely as the supervisor
    instead of exiting immediately after launching the client once.
    """
    pid = os.fork()
    if pid > 0:
        # This process, invoked from user-startup.sh, returns immediately -
        # boot continues without waiting on the supervisor.
        return

    os.setsid()

    pid2 = os.fork()
    if pid2 > 0:
        # Exit the session leader so the grandchild below can never
        # reacquire a controlling terminal (the standard double-fork
        # daemonize technique).
        os._exit(0)

    devnull_fd = os.open(os.devnull, os.O_RDWR)
    os.dup2(devnull_fd, 0)
    os.dup2(devnull_fd, 1)
    os.dup2(devnull_fd, 2)

    signal.signal(signal.SIGTERM, _handle_stop_signal)
    signal.signal(signal.SIGINT, _handle_stop_signal)

    _write_supervisor_pid()
    try:
        supervise_client(client_executable)
    finally:
        _remove_supervisor_pid()

    os._exit(0)


def run_client():
    """
    Runs the Mister Cloud Saves Client under a small supervisor that
    restarts it with backoff if it ever exits unexpectedly.
    """
    client_executable = os.path.join(CLIENT_DIR, "mister_save_client")

    if not os.path.isfile(client_executable):
        print("Mister Cloud Saves Client executable not found.")
        sys.exit(1)

    _daemonize_supervisor(client_executable)


def main():
    """
    Main function to check for updates and run the Mister Cloud Saves Client.
    """

    check_updates()
    run_client()


if __name__ == "__main__":
    main()
