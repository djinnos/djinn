#!/usr/bin/env perl
# Patch specific lines in the test file
my $file = $ARGV[0];
open my $fh, '<', $file or die "Cannot open $file: $!\n";
my @lines = <$fh>;
close $fh;

for my $i (0..$#lines) {
    my $ln = $i + 1;
    # Add arbiter_directive: None after worker_resume_note: None (line 290)
    if ($ln == 290) {
        chomp $lines[$i];
        $lines[$i] .= "\n            arbiter_directive: None,\n";
    }
}

open my $out, '>', $file or die "Cannot write $file: $!\n";
print $out @lines;
close $out;
print "Added arbiter_directive field.\n";
