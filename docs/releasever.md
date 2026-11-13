# Release override notice

`AVOCADO_RELEASEVER` is the escape hatch that overrides the release pinned in
the project lock. It always wins over the locked pin.

## The notice

When you export `AVOCADO_RELEASEVER` in a project that has a matching locked
pin, avocado prints one line per run:

```text
pin ignored: AVOCADO_RELEASEVER is set to <value>; the locked pin <pin> is not used
```

The notice never changes the release used. It only tells you the pin is being
bypassed.

## When it is silent

- The value avocado exported itself in the same run does not trigger it.
- A value equal to the locked pin does not trigger it.
- A project with no locked pin does not trigger it.
- `AVOCADO_SDK_REPO_RELEASE` and the `releasever` config key stay silent.

To stop the notice, unset `AVOCADO_RELEASEVER` and the locked pin applies
again.
