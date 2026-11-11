#!/usr/bin/env python3
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent

class InstallTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='taxe-install-test-')
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.home = self.root / 'home'
        self.home.mkdir()
        self.prefix = self.home / '.local'
        self.environment = dict(os.environ, HOME=str(self.home), TAXE_INSTALL_ROOT=str(self.prefix))
        for name in ('XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME', 'TIMBER_CHANNEL', 'TIMBER_INSTALL_ROOT', 'TIMBER_BUNDLE_PATH'):
            self.environment.pop(name, None)
        self.installer = self.root / 'install.sh'
        shutil.copyfile(ROOT / 'script/install.sh', self.installer)

    def bundle(self, version='1.29.0', channel='stable'):
        suffix = '' if channel == 'stable' else '-' + channel
        identifier = 'io.github.CodingM8s.TAXE' + ('' if channel == 'stable' else '-' + channel.capitalize())
        app = self.root / ('taxe' + suffix + '.app')
        (app / 'bin').mkdir(parents=True, exist_ok=True)
        (app / 'libexec').mkdir(exist_ok=True)
        (app / 'share/applications').mkdir(parents=True, exist_ok=True)
        (app / 'bin/taxe').write_text('#!/bin/sh\nprintf "TAXE 1.29.0\\n"\n')
        (app / 'bin/taxe').chmod(0o755)
        shutil.copyfile('/bin/true', app / 'libexec/taxe-editor')
        (app / 'libexec/taxe-editor').chmod(0o755)
        (app / 'version').write_text(version + '\n')
        (app / 'share/applications' / (identifier + '.desktop')).write_text('[Desktop Entry]\nName=TAXE\nExec=taxe %U\nTryExec=taxe\nIcon=taxe\n\n[Desktop Action NewWorkspace]\nExec=taxe --new %U\n')
        self.archive = self.root / ('taxe-linux-' + os.uname().machine + '.tar.gz')
        with tarfile.open(self.archive, 'w:gz') as archive:
            archive.add(app, arcname=app.name)
        self.checksum()
        self.environment['TAXE_BUNDLE_PATH'] = str(self.archive)
        self.environment['TAXE_CHANNEL'] = channel

    def checksum(self):
        digest = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        (self.root / 'SHA256SUMS').write_text(digest + '  ' + self.archive.name + '\n')

    def install(self, success=True):
        result = subprocess.run(['sh', str(self.installer)], env=self.environment, text=True, capture_output=True)
        self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
        return result

    def legacy(self):
        app = self.prefix / 'timber.app'
        (app / 'libexec').mkdir(parents=True)
        (app / 'libexec/timber-editor').write_text('old editor')
        (self.prefix / 'bin').mkdir()
        (self.prefix / 'bin/timber').symlink_to(app / 'bin/timber')
        config = self.home / '.config/timber'
        config.mkdir(parents=True)
        (config / 'settings.json').write_text('{"theme":"old theme"}')
        desktop = self.prefix / 'share/applications/io.github.CodingM8s.Timber.desktop'
        desktop.parent.mkdir(parents=True)
        desktop.write_text('Exec="' + str(app / 'bin/timber') + '" %U\n')
        return config, desktop

    def test_fresh_install_and_desktop_actions(self):
        self.bundle()
        self.install()
        launcher = self.prefix / 'bin/taxe'
        self.assertTrue(launcher.is_symlink())
        self.assertEqual(subprocess.check_output([str(launcher), '--version'], text=True).strip(), 'TAXE 1.29.0')
        desktop = (self.prefix / 'share/applications/io.github.CodingM8s.TAXE.desktop').read_text()
        self.assertIn('Exec="' + str(self.prefix / 'taxe.app/bin/taxe') + '" --new %U', desktop)
        self.assertFalse((self.prefix / 'bin/timber').exists())

    def test_repeat_install_preserves_previous_bundle(self):
        self.bundle()
        self.install()
        (self.prefix / 'taxe.app/old-marker').touch()
        self.install()
        self.assertEqual(len(list(self.prefix.glob('taxe.app.backup-*/old-marker'))), 1)

    def test_upgrade_migrates_settings_and_retires_only_ide_launcher(self):
        config, desktop = self.legacy()
        self.bundle()
        self.install()
        self.assertEqual((self.home / '.config/taxe/settings.json').read_text(), (config / 'settings.json').read_text())
        self.assertFalse((self.prefix / 'bin/timber').is_symlink())
        self.assertFalse(desktop.exists())
        self.assertTrue((self.prefix / 'timber.app/libexec/timber-editor').exists())

    def test_unrelated_timber_command_is_preserved(self):
        self.legacy()
        command = self.prefix / 'bin/timber'
        command.unlink()
        command.write_text('external Timber CLI')
        self.bundle()
        self.install()
        self.assertEqual(command.read_text(), 'external Timber CLI')

    def test_existing_taxe_config_is_preserved(self):
        self.legacy()
        config = self.home / '.config/taxe'
        config.mkdir()
        (config / 'settings.json').write_text('keep this')
        self.bundle()
        self.install()
        self.assertEqual((config / 'settings.json').read_text(), 'keep this')

    def test_timber_cli_config_is_not_migrated_without_old_editor(self):
        config = self.home / '.config/timber'
        config.mkdir(parents=True)
        (config / 'settings.json').write_text('CLI config')
        self.bundle()
        self.install()
        self.assertFalse((self.home / '.config/taxe').exists())

    def test_unrelated_timber_symlink_is_preserved(self):
        self.legacy()
        command = self.prefix / 'bin/timber'
        command.unlink()
        command.symlink_to('/opt/timber-cli/bin/timber')
        self.bundle()
        self.install()
        self.assertEqual(os.readlink(command), '/opt/timber-cli/bin/timber')

    def test_xdg_configuration_data_and_state_are_copied(self):
        self.legacy()
        for name in ('XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME'):
            base = self.root / name
            (base / 'timber').mkdir(parents=True)
            (base / 'timber/marker').write_text(name)
            self.environment[name] = str(base)
        self.bundle()
        self.install()
        for name in ('XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME'):
            base = Path(self.environment[name])
            self.assertEqual((base / 'taxe/marker').read_text(), name)
            self.assertEqual((base / 'timber/marker').read_text(), name)

    def test_custom_install_root_with_spaces(self):
        self.prefix = self.root / 'custom installation'
        self.environment['TAXE_INSTALL_ROOT'] = str(self.prefix)
        self.bundle()
        self.install()
        self.assertTrue((self.prefix / 'bin/taxe').is_symlink())

    def test_download_uses_the_versioned_release_and_checksums(self):
        self.bundle()
        archive_bytes = self.archive.read_bytes()
        self.archive.unlink()
        self.environment.pop('TAXE_BUNDLE_PATH')
        download = self.root / 'download.tar.gz'
        download.write_bytes(archive_bytes)
        commands = self.root / 'commands'
        commands.mkdir()
        (commands / 'curl').write_text('#!/bin/sh\nprintf \'%s\\n\' "$2" >> "$DOWNLOAD_LOG"\ncase "$2" in\n  */SHA256SUMS) cp "$DOWNLOAD_SUMS" "$4" ;;\n  *) cp "$DOWNLOAD_ARCHIVE" "$4" ;;\nesac\n')
        (commands / 'curl').chmod(0o755)
        self.environment.update(PATH=str(commands) + os.pathsep + os.environ['PATH'], DOWNLOAD_LOG=str(self.root / 'urls'), DOWNLOAD_SUMS=str(self.root / 'SHA256SUMS'), DOWNLOAD_ARCHIVE=str(download))
        self.install()
        self.assertEqual((self.root / 'urls').read_text().splitlines(), [
            'https://github.com/Coding-M8s/timber-ide/releases/download/taxe-v1.29.0/' + self.archive.name,
            'https://github.com/Coding-M8s/timber-ide/releases/download/taxe-v1.29.0/SHA256SUMS',
        ])

    def test_wrong_bundle_version_is_rejected(self):
        self.bundle(version='1.27.0')
        self.install(success=False)
        self.assertFalse((self.prefix / 'bin/taxe').exists())

    def test_damaged_archive_is_rejected(self):
        self.bundle()
        with self.archive.open('ab') as archive:
            archive.write(b'damaged')
        self.install(success=False)
        self.assertFalse((self.prefix / 'bin/taxe').exists())

    def test_missing_checksum_is_rejected(self):
        self.bundle()
        (self.root / 'SHA256SUMS').unlink()
        self.install(success=False)

    def test_preview_install_uses_separate_launcher(self):
        self.bundle(channel='preview')
        self.install()
        self.assertTrue((self.prefix / 'bin/taxe-preview').is_symlink())
        self.assertTrue((self.prefix / 'share/applications/io.github.CodingM8s.TAXE-Preview.desktop').exists())

if __name__ == '__main__':
    unittest.main()
