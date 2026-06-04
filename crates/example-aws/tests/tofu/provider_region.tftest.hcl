# Provider configuration flows to the resource.
#
# A file-level `provider` block configures the example provider's optional
# `region`. `configure` turns it into the shared client, which stamps the region
# onto every bucket. (The default-region case — no provider block — is covered
# by lifecycle.tftest.hcl asserting us-east-1.)

provider "aws" {
  region = "eu-west-1"
}

run "configured_region_reaches_resource" {
  command = apply

  assert {
    condition     = aws_s3_bucket.test.region == "eu-west-1"
    error_message = "the configured provider region should reach the resource handler"
  }
}
