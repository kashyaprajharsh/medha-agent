use super::*;

/// The three kinds are removed wherever they appear, and nothing that only
/// resembles a secret by name or by length is touched.
#[test]
fn secrets_are_removed_and_look_alikes_are_left_alone() {
    let secrets = Secrets::new(["held-provider-key-0001".to_owned(), "short".to_owned()]);
    let text = "\
using held-provider-key-0001 for the call
token sk-proj-abcdefghijklmnopqrstuvwx1234 printed
AKIAABCDEFGHIJKLMNOP in a log
DB_PASSWORD=hunter2hunter2X
export GITHUB_TOKEN='Zx9vQ2mK8pL4nR7tW1yB'
api.key: s3cr3t-Value-123456
MAX_TOKENS=4096
CACHE_KEY=/var/cache/app
PASSWORD_FILE=$HOME/.pw
API_KEY=changeme
KEYBOARD=us-international-x
PWD=/Users/me/projects/demo
short is not removed
-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEA
-----END RSA PRIVATE KEY-----
done";
    let redacted = secrets.redact(text);
    assert_eq!(
        redacted,
        "\
using [secret removed] for the call
token [secret removed] printed
[secret removed] in a log
DB_PASSWORD=[secret removed]
export GITHUB_TOKEN='[secret removed]'
api.key: [secret removed]
MAX_TOKENS=4096
CACHE_KEY=/var/cache/app
PASSWORD_FILE=$HOME/.pw
API_KEY=changeme
KEYBOARD=us-international-x
PWD=/Users/me/projects/demo
short is not removed
[secret removed]
done"
    );
}

#[test]
fn text_without_secrets_is_returned_as_it_was() {
    let text = "cargo test\ntest result: ok. 12 passed\n";
    assert!(matches!(Secrets::default().redact(text), Cow::Borrowed(_)));
}
