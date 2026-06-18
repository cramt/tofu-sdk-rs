# The configuration under test for the `tofu test` contract suite. The provider
# is supplied via `dev_overrides` (see the Rust runner in tofu_test.rs), so no
# `init` runs. The bucket name is a variable so `run` blocks can drive renames
# (which exercise the `force_new` replacement path).

terraform {
  required_providers {
    aws = {
      source = "example/aws"
    }
  }
}

variable "bucket_name" {
  type    = string
  default = "my-bucket"
}

resource "aws_s3_bucket" "test" {
  name = var.bucket_name
}

# Read-only lookups, exercised by data_source.tftest.hcl. Independent of the
# managed bucket above (their own addresses), so they do not affect the resource
# lifecycle assertions in the other test files.

# Singular: looked up by the unique `arn` (exclusive key) -> one object.
data "aws_s3_bucket" "by_arn" {
  arn = "arn:aws:s3:::looked-up"
}

# Plural: looked up by the generic `name` (shared key) -> a `results` list.
data "aws_s3_buckets" "by_name" {
  name = "team"
}

# Exercise the provider-defined function `arn_for`: functions.tftest.hcl asserts
# on this output, which forces OpenTofu to call the function over the plugin
# protocol (CallFunction).
output "arn_for_bucket" {
  value = provider::aws::arn_for(var.bucket_name)
}
