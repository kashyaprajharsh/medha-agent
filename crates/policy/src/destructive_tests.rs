use super::*;
use shell_reader::{Shell, read_as};

fn loss_of(line: &str) -> Option<&'static str> {
    let bash = shell_reader::read(line);
    let powershell = read_as(line, Shell::PowerShell);
    loss(&[&bash, &powershell], line)
}

/// Every kind of loss is named on its card, in bash, PowerShell and cmd alike.
#[test]
fn each_way_of_losing_something_is_named() {
    for (kind, lines) in [
        (
            FILES,
            &[
                "rm notes.txt",
                "rm -rf gui-app/app",
                "rm -- -r file",
                "cd gui-app && rm -rf app",
                "env X=1 rm -rf build",
                "ls | xargs rm",
                "unlink a.txt",
                "shred -u secrets.txt",
                "trash build",
                "rmdir empty",
                "find . -name '*.py' -delete",
                "find . -exec rm {} ;",
                "Remove-Item gui-app -Recurse -Force",
                "ri old.txt",
                "del /s /q src",
                "rd /s /q src",
                "erase notes.txt",
                "Clear-Content log.txt",
            ][..],
        ),
        (
            GIT,
            &[
                "git clean -fdx",
                "git clean -f",
                "git reset --hard HEAD~1",
                "git checkout -- src",
                "git checkout .",
                "git restore src",
                "git push --force origin main",
                "git push -f",
                "git push origin +main",
                "git push origin --delete feature",
                "git branch -D feature",
                "git stash drop",
                "git stash clear",
                "git -C repo reset --hard",
            ][..],
        ),
        (
            DATABASE,
            &[
                "psql -c \"DROP TABLE users\"",
                "sqlite3 app.db 'delete from users'",
                "mysql -e 'TRUNCATE TABLE logs'",
                "dropdb production",
                "redis-cli FLUSHALL",
                "mongosh --eval 'db.dropDatabase()'",
                "rails db:drop",
                "python manage.py flush",
                "npx prisma migrate reset",
            ][..],
        ),
        (
            RESOURCES,
            &[
                "docker rm -f web",
                "docker rmi app:latest",
                "docker volume rm data",
                "docker system prune -a",
                "docker compose down -v",
                "kubectl delete pod web",
                "helm uninstall web",
                "terraform destroy",
                "aws s3 rm s3://bucket --recursive",
                "gcloud compute instances delete vm-1",
            ][..],
        ),
        (
            SYSTEM,
            &[
                "dd if=/dev/zero of=/dev/disk2",
                "diskutil eraseDisk APFS X disk2",
                "mkfs.ext4 /dev/sdb1",
                "Format-Volume -DriveLetter D",
                "diskpart",
                "vssadmin delete shadows /all",
                "reg delete HKCU\\Software\\App /f",
                "cipher /w:C",
            ][..],
        ),
    ] {
        for line in lines {
            assert_eq!(loss_of(line), Some(kind), "{line}");
        }
    }
}

/// Everyday commands that lose nothing never ask for this.
#[test]
fn everyday_commands_lose_nothing() {
    for line in [
        "ls -la",
        "git status",
        "git push origin main",
        "git commit -m 'tidy the readme'",
        "git checkout feature",
        "git restore --staged src/main.rs",
        "git branch feature",
        "git clean -n",
        "cargo test",
        "npm run build",
        "docker ps",
        "docker compose down",
        "kubectl get pods",
        "terraform plan",
        "find . -name '*.rs'",
        "Get-ChildItem -Recurse",
        "mkdir -p out",
        "cp a.txt b.txt",
        "mv a.txt b.txt",
    ] {
        assert_eq!(loss_of(line), None, "{line}");
    }
}
