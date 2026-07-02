#!/usr/bin/env perl
use strict;
use warnings;

my $create_file = "server/crates/djinn-control-plane/src/tools/proposal_tools/create.rs";
my $mod_file = "server/crates/djinn-control-plane/src/tools/proposal_tools/mod.rs";

# Resolve mod.rs: remove the only remaining conflict block (both sides move to create.rs)
my $mod = read_file($mod_file);
if ($mod =~ s/<<<<<<< HEAD\n(.*?)\n=======\n(.*?)\n>>>>>>> origin\/main\n//s) {
    print "Resolved mod.rs tool-methods conflict.\n";
} else {
    die "mod.rs conflict block not found";
}
write_file($mod_file, $mod);

# Resolve create.rs: merge the only remaining conflict block (keep both sides)
my $create = read_file($create_file);
if ($create =~ s/<<<<<<< HEAD\n(.*?)\n=======\n(.*?)\n>>>>>>> origin\/main\n/join_conflicts($1, $2)/s) {
    print "Resolved create.rs tool-methods conflict.\n";
} else {
    die "create.rs conflict block not found";
}
write_file($create_file, $create);

# Verify no markers remain
for my $f ($create_file, $mod_file) {
    my $c = read_file($f);
    die "Leftover conflict markers in $f" if $c =~ /<<<<<<< HEAD|=======|>>>>>>> origin\/main/;
}

print "All conflicts resolved.\n";

sub join_conflicts {
    my ($head, $main) = @_;
    # HEAD side is missing the closing `}` for proposal_remove_target.
    $head =~ s/\s+$//;
    $head .= "\n    }\n";
    return $head . "\n" . $main . "\n";
}

sub read_file {
    my ($f) = @_;
    open my $fh, '<', $f or die "read $f: $!";
    local $/;
    return <$fh>;
}

sub write_file {
    my ($f, $c) = @_;
    open my $fh, '>', $f or die "write $f: $!";
    print $fh $c;
}
