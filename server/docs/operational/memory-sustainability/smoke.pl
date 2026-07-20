#!/usr/bin/env perl
# Cluster-independent integration smoke for the operational protocol.
use strict;
use warnings;
use Getopt::Long qw(GetOptions);
use File::Path qw(make_path);

my $here = __FILE__;
$here =~ s{/[^/]+$}{};
my $out;
GetOptions('output-dir=s' => \$out) or die "usage: perl smoke.pl --output-dir DIR\n";
die "usage: perl smoke.pl --output-dir DIR\n" unless $out;
make_path($out);
my $fixture_dir = "$out/fixtures";
my $driver_raw = "$out/driver-raw.jsonl";
my $eval_input = "$out/evaluator-input.json";
my $eval_json = "$out/evaluation.json";
my $eval_md = "$out/evaluation.md";

sub run {
    system @_;
    die "command failed (exit " . ($? >> 8) . ")\n" if $?;
}

run('perl', "$here/fixtures/generate.pl", '--profile', 'smoke', '--output-dir', $fixture_dir, '--validate');
run('perl', "$here/driver/memory_workload.pl", '--fake', '--profile', 'smoke',
    '--output', $driver_raw, '--t0-seconds', '0', '--t1-seconds', '0',
    '--burst-seconds', '0', '--t2-seconds', '0', '--request-count', '6');
run('perl', "$here/evaluator/adapt_driver_jsonl.pl", '--candidate-raw', $driver_raw,
    '--candidate-image', 'synthetic/fake-driver', '--candidate-fixture-manifest',
    "$here/fixtures/manifest.json", '--output', $eval_input);
run('perl', "$here/evaluator/evaluate.pl", '--input', $eval_input,
    '--json-out', $eval_json, '--report-out', $eval_md);
print "PASS: fake collection and evaluation completed\nraw=$driver_raw\ninput=$eval_input\nresult=$eval_json\nreport=$eval_md\n";
