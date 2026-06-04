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

# Read-only lookup, exercised by data_source.tftest.hcl. Independent of the
# managed bucket above (its own type-name address), so it does not affect the
# resource lifecycle assertions in the other test files.
data "aws_s3_bucket" "lookup" {
  name = "looked-up"
}
