# Provider-defined function, exercised end to end through the real engine.
#
# `provider::aws::arn_for(name)` is a pure function (no provider config or
# state); OpenTofu calls our plugin's GetFunctions/CallFunction over the plugin
# protocol to evaluate it in the assert condition below.

run "arn_for_builds_a_bucket_arn" {
  command = plan

  assert {
    condition     = output.arn_for_bucket == "arn:aws:s3:::my-bucket"
    error_message = "arn_for should build the bucket ARN from the name"
  }

  assert {
    condition     = output.joined_parts == "a-b-c"
    error_message = "the variadic join function should join the trailing parts"
  }
}
