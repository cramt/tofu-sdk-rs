# Data source reads, both cardinalities, via the ReadDataSource RPC.
#
# - `data "aws_s3_bucket"` is singular: looked up by the unique `arn`
#   (search_key exclusive), it resolves to one object whose other attributes are
#   computed (name recovered from the arn, region from the provider client).
# - `data "aws_s3_buckets"` is plural: looked up by the generic `name`
#   (search_key shared), it resolves to a `results` list of matching objects.

run "singular_lookup_by_arn" {
  command = apply

  assert {
    condition     = data.aws_s3_bucket.by_arn.name == "looked-up"
    error_message = "singular data source should recover the name from the arn"
  }

  assert {
    condition     = data.aws_s3_bucket.by_arn.region == "us-east-1"
    error_message = "singular data source should read the default provider region"
  }
}

run "plural_lookup_by_name" {
  command = apply

  assert {
    condition     = length(data.aws_s3_buckets.by_name.results) == 2
    error_message = "plural data source should return the list of matches"
  }

  assert {
    condition     = data.aws_s3_buckets.by_name.results[0].name == "team"
    error_message = "first match should be the queried name"
  }

  assert {
    condition     = data.aws_s3_buckets.by_name.results[1].name == "team-staging"
    error_message = "second synthetic match should be derived from the queried name"
  }

  assert {
    condition     = data.aws_s3_buckets.by_name.results[0].arn == "arn:aws:s3:::team"
    error_message = "each result should carry its computed arn"
  }
}
