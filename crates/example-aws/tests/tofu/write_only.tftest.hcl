# Write-only attribute semantics, end to end through the real plugin protocol.
#
# `aws_locker.secret` is declared write_only: its value is supplied in config and
# reaches the provider's `create` handler (which sets the computed `has_secret`
# witness), but it must never be persisted to state. OpenTofu enforces the
# null-in-state rule itself, so this run also proves our `write_only: true`
# schema flag is accepted by the engine.

run "write_only_secret_reaches_handler_but_not_state" {
  command = apply

  # The handler observed the supplied secret...
  assert {
    condition     = aws_locker.test.has_secret == true
    error_message = "create should have received the write-only secret from config"
  }

  # ...but the write-only value itself is null in state.
  assert {
    condition     = aws_locker.test.secret == null
    error_message = "a write-only attribute must never be persisted to state"
  }
}
