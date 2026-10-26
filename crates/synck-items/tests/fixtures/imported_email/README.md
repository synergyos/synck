# Imported-email fixtures

`representative.eml` is a repository-authored RFC822/MIME fixture.

`ascii.msg.b64` and `test_email_3.msg.b64` are base64 encodings of the corresponding `data/`
fixtures from `msg_parser` 0.3.6. They are redistributed under the MIT license in
`LICENSE.msg_parser`. Tests decode them to the original binary MSG bytes before parsing.

Expected source SHA-256 values:

- `ascii.msg`: `a6fc2f852e2ad1b89cf43db7b653d2df96aacca9daf1ee12250f1f3f56e211e2`
- `test_email_3.msg`: `29e4b20323cdd95bcea6040aed7c69becee571cbc0d8855d8853b5c03646d375`
