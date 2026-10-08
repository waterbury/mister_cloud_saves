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
# https://github.com/waterbury/mister_cloud_saves


import os
import sys
import signal
import subprocess
import configparser
import shutil
import time
import hashlib
import requests  # type: ignore

DEFAULT_SERVER_URL = "https://mister-cloud-saves.tuxprint.com"
GH_REPO_URL = "https://github.com/waterbury/mister_cloud_saves"
LATEST_RELEASE_URL = f"{GH_REPO_URL}/releases/latest"

RAW_URL_BASE = "https://raw.githubusercontent.com/waterbury/mister_cloud_saves"
SCRIPT_RAW_URL = f"{RAW_URL_BASE}/refs/heads/main/scripts/cloud_saves.sh"
LAUNCHER_SCRIPT_URL = f"{RAW_URL_BASE}/refs/heads/main/scripts/cloud_saves_launcher.sh"
MISTER_PATH = "/media/fat"
CLIENT_DIR = os.path.join(MISTER_PATH, "cloud_saves")
MISTER_LINUX_DIR = os.path.join(MISTER_PATH, "linux/")


def get_user_id(server_url):
    """
    Takes server_url as parameter to request a new user ID if config does not exist.

    :return: User ID
    """

    if is_config_existing():
        config = read_config()
        return config.get("User", "user_id")

    response = requests.get(f"{server_url}/generate_user_id", timeout=30)
    if response.status_code == 200:
        return response.text.strip()

    print("Error generating user ID")
    sys.exit(1)


def get_server_url():
    """
    Gets server URL from config or prompts user for it.

    :return: Server URL
    """

    if is_config_existing():

        config = read_config()
        return config.get("Server", "server_url")

    while True:
        print("The default server URL is:", DEFAULT_SERVER_URL)
        user_input = input("Enter server URL or press Enter to use default: ").strip()

        if user_input == "":
            return DEFAULT_SERVER_URL
        if is_valid_url(user_input):
            return user_input

        print("Invalid URL, please ensure the server is reachable.")


def is_valid_url(url):
    """
    Checks if the provided URL is valid by making a GET request.

    :param url: URL to validate
    :return: True if URL is valid, False otherwise
    """

    health_url = f"{url}/health"

    try:
        response = requests.get(health_url, timeout=30)
        return response.status_code == 200
    except requests.RequestException:
        return False


def create_config(user_id, server_url):
    """
    Creates configuration file with user ID and server URL.

    :param user_id: User ID to store in config
    :param server_url: Server URL to store in config
    """
    print("Creating configuration file...")

    config = configparser.ConfigParser()
    config["User"] = {"user_id": user_id}
    config["Server"] = {"server_url": server_url}

    with open(
        os.path.join(MISTER_PATH, "cloud_saves.ini"), "w", encoding="utf-8"
    ) as configfile:
        config.write(configfile)


def is_config_existing():
    """
    Checks if configuration file exists.

    :return: True if config file exists, False otherwise
    """

    return os.path.isfile(os.path.join(MISTER_PATH, "cloud_saves.ini"))


def read_config():
    """
    Reads configuration file.

    :return: ConfigParser object with loaded config
    """

    config = configparser.ConfigParser()
    config.read(os.path.join(MISTER_PATH, "cloud_saves.ini"))
    return config


def is_client_installed():
    """
    Checks if the Mister Cloud Saves Client is installed.

    :return: True if client is installed, False otherwise
    """

    client_bin = os.path.join(CLIENT_DIR, "mister_save_client")
    ini_file = os.path.join(MISTER_PATH, "cloud_saves.ini")

    return os.path.isfile(client_bin) and os.path.isfile(ini_file)


def get_tag_and_latest_release_url():
    """
    Gets the latest release download URL from GitHub.

    The tag is read from where the releases/latest page redirects to rather
    than from api.github.com, which allows only 60 requests an hour per
    public IP address and so fails when other devices on the same network
    have used them up.

    :return: Download URL string
    """

    try:
        response = requests.get(LATEST_RELEASE_URL, timeout=30, allow_redirects=False)
    except requests.RequestException as error:
        print(f"Error fetching latest release info: {error}")
        sys.exit(1)

    location = response.headers.get("Location", "")

    if response.status_code in (301, 302, 303, 307, 308) and "/releases/tag/" in location:
        tag_name = location.rstrip("/").rsplit("/", 1)[-1]
        return (f"{GH_REPO_URL}/releases/download/{tag_name}/client.tar.xz", tag_name)

    print(
        f"Error fetching latest release info (HTTP {response.status_code} "
        f"from {LATEST_RELEASE_URL})"
    )
    sys.exit(1)


def fetch_launcher_script():
    """
    Downloads the Mister Cloud Saves Launcher Script.
    """
    print("Downloading Mister Cloud Saves Launcher Script...")

    response = requests.get(LAUNCHER_SCRIPT_URL, timeout=30)
    if response.status_code == 200:
        with open(
            os.path.join(CLIENT_DIR, "cloud_saves_launcher.sh"),
            "wb",
        ) as file:
            file.write(response.content)
    else:
        print("Error downloading launcher script")
        sys.exit(1)


def fetch_client(download_url=None):
    """
    Downloads the Mister Cloud Saves Client.
    """
    print("Downloading Mister Cloud Saves Client...")

    if download_url is None:
        download_url, _ = get_tag_and_latest_release_url()

    response = requests.get(download_url, timeout=30)
    if response.status_code == 200:
        with open(
            os.path.join("/tmp", "client.tar.xz"),
            "wb",
        ) as file:
            file.write(response.content)
    else:
        print(f"Error downloading client (HTTP {response.status_code} from {download_url})")
        sys.exit(1)


def get_client_version():
    """
    Gets the installed Mister Cloud Saves Client version.

    :return: Version string
    """

    client_bin = os.path.join(CLIENT_DIR, "mister_save_client")

    if not os.path.isfile(client_bin):
        return "not installed"

    result = subprocess.run(
        [client_bin, "--version"],
        capture_output=True,
        text=True,
        check=True,
    )

    version = result.stdout.strip().split()[-1]

    return version


def extract_client():
    """
    Extracts the client archive to the client directory.
    """
    print("Extracting Mister Cloud Saves Client...")

    temp_archive_path = os.path.join("/tmp", "client.tar.xz")

    if os.path.isfile(temp_archive_path):
        if not os.path.isdir(CLIENT_DIR):
            os.makedirs(CLIENT_DIR)

        shutil.unpack_archive(temp_archive_path, CLIENT_DIR, format="xztar")

    else:
        print("Client archive not found")
        sys.exit(1)


def run_initial_sync():
    """
    Runs an initial one-shot sync of the Mister Cloud Saves Client.
    """
    print("Running initial sync...")

    client_bin = os.path.join(CLIENT_DIR, "mister_save_client")

    if not os.path.isfile(client_bin):
        print("Client binary not found")
        sys.exit(1)

    subprocess.run([client_bin, "--one-shot"], check=True)


def reboot_system():
    """
    Reboots the system after a countdown.
    """

    for i in range(5, 0, -1):
        print(f"Rebooting in {i} seconds...", end="\r")
        time.sleep(1)

    print("Rebooting now!            ")

    subprocess.run(["reboot"], check=True)


def update_user_scripts_install():
    """
    Updates user-startup.sh to include Mister Cloud Saves Client on startup.

    """
    print("Updating user-startup.sh to include Mister Cloud Saves Client...")

    script_path = os.path.join(MISTER_LINUX_DIR, "user-startup.sh")

    if not os.path.isfile(script_path):
        shutil.copy(
            os.path.join(MISTER_LINUX_DIR, "_user-startup.sh"),
            script_path,
        )

    with open(script_path, "a", encoding="utf-8") as file:
        file.write(f"\n{CLIENT_DIR}/cloud_saves_launcher.sh\n")


def update_user_scripts_migrate():
    """
    Migrates user-startup.sh to ensure Mister Cloud Saves Client using the current command
    """
    print(
        "Migrating user-startup.sh to ensure Mister Cloud Saves Client uses the correct command..."
    )

    script_path = os.path.join(MISTER_LINUX_DIR, "user-startup.sh")

    legacy_command = f"{CLIENT_DIR}/mister_save_client &"
    current_command = f"{CLIENT_DIR}/cloud_saves_launcher.sh"

    if os.path.isfile(script_path):
        with open(script_path, "r", encoding="utf-8") as file:
            lines = file.readlines()

        with open(script_path, "w", encoding="utf-8") as file:
            for line in lines:
                if legacy_command in line:
                    file.write(line.replace(legacy_command, current_command))
                else:
                    file.write(line)


def update_user_scripts_remove():
    """
    Updates user-startup.sh to remove Mister Cloud Saves Client on startup.
    """
    print("Updating user-startup.sh to remove Mister Cloud Saves Client...")

    script_path = os.path.join(MISTER_LINUX_DIR, "user-startup.sh")

    if os.path.isfile(script_path):
        with open(script_path, "r", encoding="utf-8") as file:
            lines = file.readlines()

        with open(script_path, "w", encoding="utf-8") as file:
            for line in lines:
                if CLIENT_DIR not in line:
                    file.write(line)


def get_downloader_ini():
    """
    Reads the downloader.ini file

    :return: ConfigParser object with loaded downloader config
    """

    config = configparser.ConfigParser()
    config.read(os.path.join(MISTER_PATH, "downloader.ini"))
    return config


def remove_from_updater():
    """
    Removes Mister Cloud Saves database update from downloader.ini if present.

    Earlier versions of this script registered the upstream update database
    there, which has update_all replace this script, the launcher and the
    client with the upstream builds. Updates now only come from this script's
    own update option, so the entry is removed on install and update as well
    as on uninstall.
    """

    config = get_downloader_ini()

    if "mister_cloud_saves" in config.sections():
        print("Removing Mister Cloud Saves database update from downloader.ini...")

        config.remove_section("mister_cloud_saves")

        with open(
            os.path.join(MISTER_PATH, "downloader.ini"),
            "w",
            encoding="utf-8",
        ) as configfile:
            config.write(configfile)


def discard_pending_update_archive():
    """
    Deletes a client archive left in the updates directory by update_all.

    Launchers from before the downloader.ini entry was dropped extract that
    archive over the installed client at the next boot, so it has to be gone
    before the reboot that follows an install or update.
    """

    update_file = os.path.join(CLIENT_DIR, "updates", "client.tar.xz")
    if os.path.isfile(update_file):
        print("Discarding client archive left by update_all...")
        os.remove(update_file)


def _read_pid_file(pid_path):
    """
    Reads a pid file, tolerating it being absent or containing garbage left
    over from an unclean shutdown.

    :return: PID as integer, or 0 if there is none
    """

    if not os.path.isfile(pid_path):
        return 0

    try:
        with open(pid_path, "r", encoding="utf-8") as pid_file:
            return int(pid_file.read().strip())
    except (OSError, ValueError):
        return 0


def _process_is_alive(pid):
    if pid == 0:
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def save_client_pid():
    """
    Retrieves the PID of the running Mister Cloud Saves Client.

    :return: PID as integer
    """

    return _read_pid_file(os.path.join("/var/run", "mister_save_client.pid"))


def supervisor_pid():
    """
    Retrieves the PID of the launcher's supervisor process, if the currently
    installed launcher has one. Older launchers (before the client was
    supervised) never write this file, so its absence is normal rather than
    an error.

    :return: PID as integer
    """

    return _read_pid_file(os.path.join("/var/run", "mister_save_client_launcher.pid"))


def stop_client_process():
    """
    Stops the running Mister Cloud Saves Client, and the supervisor that
    would otherwise immediately restart it.

    The supervisor must be stopped first: it exists specifically to bring
    the client back if it ever exits, so signaling the client alone would
    just have the supervisor relaunch it - possibly mid-update, racing the
    new binary being extracted on top of the one that just got killed.
    """

    sup_pid = supervisor_pid()
    if sup_pid and _process_is_alive(sup_pid):
        print(f"Stopping Mister Cloud Saves Client supervisor with PID {sup_pid}...")
        try:
            os.kill(sup_pid, signal.SIGTERM)
        except ProcessLookupError:
            pass

        # Give the supervisor a moment to forward the signal to the client
        # and exit cleanly before anything starts overwriting the binary it
        # was running.
        for _ in range(50):  # up to ~5s
            if not _process_is_alive(sup_pid):
                break
            time.sleep(0.1)

    pid = save_client_pid()
    if not _process_is_alive(pid):
        if not sup_pid:
            print("Mister Cloud Saves Client is not running.")
        return

    print(f"Stopping Mister Cloud Saves Client with PID {pid}...")

    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass


def change_server():
    """
    Changes the server URL in the configuration file.
    """

    stop_client_process()

    if is_config_existing():
        config_path = os.path.join(MISTER_PATH, "cloud_saves.ini")
        os.remove(config_path)

    server_url = get_server_url()
    user_id = get_user_id(server_url)
    create_config(user_id, server_url)

    run_initial_sync()
    reboot_system()


def install():
    """
    The installer function for Mister Cloud Saves Client.
    """
    print("Installing Mister Cloud Saves Client...")

    stop_client_process()
    fetch_launcher_script()
    fetch_client()
    extract_client()

    if not is_config_existing():
        server_url = get_server_url()
        user_id = get_user_id(server_url)
        create_config(user_id, server_url)
    else:
        print("Configuration file already exists. Skipping creation.")

    update_user_scripts_install()
    remove_from_updater()
    discard_pending_update_archive()

    run_initial_sync()
    reboot_system()


def uninstall():
    """
    Uninstalls the Mister Cloud Saves Client.
    """
    print("Uninstalling Mister Cloud Saves Client...")

    update_user_scripts_remove()
    remove_from_updater()
    stop_client_process()

    if os.path.isdir(CLIENT_DIR):
        shutil.rmtree(CLIENT_DIR)

    config_path = os.path.join(MISTER_PATH, "cloud_saves.ini")
    if os.path.isfile(config_path):
        os.remove(config_path)

    print("Mister Cloud Saves Client uninstalled.")

    reboot_system()


def update_self():
    """
    Updates the Mister Cloud Saves installer script (this one).
    """
    print("Checking for installer script updates...")

    tmp_dir = os.path.abspath("/media/fat/cloud_saves/tmp/")
    tmp_path = os.path.join(tmp_dir, "cloud_saves.sh")
    if not os.path.isdir(tmp_dir):
        os.makedirs(tmp_dir)

    response = requests.get(SCRIPT_RAW_URL, timeout=30)
    installer_path = os.path.join("/media/fat/Scripts", "cloud_saves.sh")

    if response.status_code == 200:
        tmp_hash = hashlib.sha256(response.content).hexdigest()

        with open(installer_path, "rb") as current_file:
            current_hash = hashlib.sha256(current_file.read()).hexdigest()

        if tmp_hash == current_hash:
            print("Script is already up to date.")
            return

        print("Updating script...")
        with open(
            tmp_path,
            "wb",
        ) as file:
            file.write(response.content)

        shutil.move(
            tmp_path,
            os.path.join("/media/fat/Scripts", "cloud_saves.sh"),
        )

        print("Installer script updated successfully.")
        print("Restarting installer...")

        os.execv(installer_path, [installer_path, "--update-client-only"])

    else:
        print("Error updating script")
        sys.exit(1)


def update():
    """
    Updates the Mister Cloud Saves Client.
    """

    print("Checking for Mister Cloud Saves Client updates...")

    current_version = get_client_version()
    client_download_url, latest_version = get_tag_and_latest_release_url()
    print(f"Current installed version: {current_version}")
    print(f"Latest available version: {latest_version}")

    # update_all must not manage this install (see remove_from_updater)
    remove_from_updater()
    discard_pending_update_archive()
    update_user_scripts_migrate()

    fetch_launcher_script()

    if current_version == latest_version:
        print("Mister Cloud Saves Client is already up to date.")
        return

    print("Updating to the latest version...")

    stop_client_process()

    fetch_client(client_download_url)
    extract_client()

    run_initial_sync()

    reboot_system()


if __name__ == "__main__":
    ACTION = None

    if len(sys.argv) >= 2:
        ACTION = sys.argv[1]
    else:
        print("Mister Cloud Saves Script")
        print("=========================")

        if not is_client_installed():
            print("Client is not installed.")
            ACTION = "--install"
        else:
            while True:
                print("please choose an action:")
                print("1. Install")
                print("2. Update")
                print("3. Uninstall")
                print("4. Change Server")
                print("5. Exit")

                choice = input("Enter choice (1|2|3|4|5): ").strip()

                if choice == "1":
                    ACTION = "--install"
                elif choice == "2":
                    ACTION = "--update"
                elif choice == "3":
                    ACTION = "--uninstall"
                elif choice == "4":
                    ACTION = "--change-server"
                elif choice == "5":
                    print("Exiting.")
                    sys.exit(0)
                else:
                    print("Invalid choice. Please enter 1, 2, 3, 4, or 5.")
                    continue
                break

    if ACTION.lower() == "--install":
        install()
    elif ACTION.lower() == "--update":
        update_self()
        update()
    elif ACTION.lower() == "--update-client-only":
        update()
    elif ACTION.lower() == "--uninstall":
        uninstall()
    elif ACTION.lower() == "--change-server":
        change_server()
    else:
        print(
            "Invalid action. Use '--install', '--update', '--uninstall', or '--change-server'."
        )
        sys.exit(1)
