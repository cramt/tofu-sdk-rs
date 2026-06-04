# Data source read.
#
# The `data "aws_s3_bucket"` block computes its attributes through the
# ReadDataSource RPC: the arn is derived from the queried name and the region
# comes from the configured provider client (the same meta the resource uses).
# No managed resource is involved in these assertions.

run "lookup_computes_attributes" {
  command = apply

  assert {
    condition     = data.aws_s3_bucket.lookup.arn == "arn:aws:s3:::looked-up"
    error_message = "data source should compute the arn from the queried name"
  }

  assert {
    condition     = data.aws_s3_bucket.lookup.region == "us-east-1"
    error_message = "data source should read the default provider region"
  }
}
