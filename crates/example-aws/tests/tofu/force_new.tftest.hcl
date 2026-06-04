# `force_new` replacement.
#
# `name` is a `force_new` attribute, so changing it must replace the resource
# rather than update it in place. On a replacement the planning engine marks
# every computed attribute unknown, so the create handler runs again and writes
# last_action = "created". An in-place update would instead run the update
# handler (last_action = "updated"), so asserting "created" after a rename is a
# clean, protocol-level signal that the change forced replacement.

run "create_alpha" {
  command = apply

  variables {
    bucket_name = "alpha"
  }

  assert {
    condition     = aws_s3_bucket.test.arn == "arn:aws:s3:::alpha"
    error_message = "arn should track the initial bucket name"
  }

  assert {
    condition     = aws_s3_bucket.test.last_action == "created"
    error_message = "the create handler should run on first apply"
  }
}

run "rename_forces_replacement" {
  command = apply

  variables {
    bucket_name = "beta"
  }

  assert {
    condition     = aws_s3_bucket.test.arn == "arn:aws:s3:::beta"
    error_message = "the recreated bucket should recompute its arn from the new name"
  }

  assert {
    condition     = aws_s3_bucket.test.last_action == "created"
    error_message = "changing the force_new name should replace (run create), not update"
  }
}
