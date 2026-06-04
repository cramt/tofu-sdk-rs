# Create lifecycle and plan idempotency.
#
# Drives a real apply through the plugin protocol (ValidateResourceConfig,
# PlanResourceChange with computed -> unknown, ApplyResourceChange create, and
# the cty msgpack codec both ways), then re-plans to confirm the computed
# values are stable. `tofu test` tears the bucket down automatically afterwards,
# which exercises the destroy path (UpgradeResourceState + ReadResource +
# ApplyResourceChange delete).

run "create_computes_attributes" {
  command = apply

  assert {
    condition     = aws_s3_bucket.test.arn == "arn:aws:s3:::my-bucket"
    error_message = "provider should compute the arn from the bucket name"
  }

  assert {
    condition     = aws_s3_bucket.test.region == "us-east-1"
    error_message = "with no provider config the region should default to us-east-1"
  }

  assert {
    condition     = aws_s3_bucket.test.last_action == "created"
    error_message = "the create handler should run on first apply"
  }
}

run "replan_is_a_no_op" {
  command = plan

  # Re-planning the unchanged config must not perturb the computed values: if
  # planning wrongly diffed them this would fail (the engine keeps known
  # computed values across a no-op plan rather than marking them unknown).
  assert {
    condition     = aws_s3_bucket.test.arn == "arn:aws:s3:::my-bucket"
    error_message = "computed arn should be stable across a no-op re-plan"
  }

  assert {
    condition     = aws_s3_bucket.test.last_action == "created"
    error_message = "a no-op re-plan should not re-run a handler"
  }
}
