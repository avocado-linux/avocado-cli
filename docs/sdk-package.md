# SDK package keys

In `sdk.compile.<section>.package` these optional keys shape the RPM that is
built:

| Key | Type | Effect |
|-----|------|--------|
| `provides` | list of strings | One `Provides:` line per entry |
| `epoch` | integer | `Epoch:` in the spec |
| `pre_install` | script string | `%pre` |
| `post_install` | script string | `%post` |
| `pre_uninstall` | script string | `%preun` |
| `post_uninstall` | script string | `%postun` |

`AutoReqProv` stays off.

## Example

```yaml
sdk:
  compile:
    probe:
      package:
        epoch: 2
        provides: ["probe-cap = 1.0"]
        post_install: echo "stamp $(date +%s)"
```

## Percent escaping

A `%` inside a scriptlet line or a provide is escaped so rpm reads it
literally. Write `date +%s` and `100%s` as is; the built RPM keeps them. A line
that starts with `%` is the exception: rpm expands macros before it looks for
section headers, so it would end the scriptlet. Such a line is rejected.

## Rejected input

- A provide containing a newline or other control character, or an empty
  provide, is rejected with an error naming the entry.
- A scriptlet containing a line exactly equal to `SPEC_EOF` is rejected.
- A scriptlet containing a line whose first non-blank character is `%` is
  rejected with the line quoted. Start the line with another character.

## Verify

```sh
rpm -qp --provides pkg.rpm
rpm -qp --qf '%{EPOCH}\n' pkg.rpm
rpm -qp --scripts pkg.rpm
```

## Unknown keys

An unknown key in a package section is only a `config validate` warning, as in
every other section: it is ignored, and the warning carries a did-you-mean.
