#!/usr/bin/env perl
# Patch specific lines in the test file
my $file = $ARGV[0];
open my $fh, '<', $file or die "Cannot open $file: $!\n";
my @lines = <$fh>;
close $fh;

# Line numbers are 1-based; arrays are 0-based
for my $i (0..$#lines) {
    my $ln = $i + 1;
    if ($ln == 4642 || $ln == 4796) {
        $lines[$i] =~ s/ReplyLoopHarness::new\(\)/ReplyLoopHarness::new_with_worker_prompt()/;
    }
}

open my $out, '>', $file or die "Cannot write $file: $!\n";
print $out @lines;
close $out;
print "Patched lines 4642 and 4796.\n";
