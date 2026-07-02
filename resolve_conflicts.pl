perl -e '
use strict;
use warnings;
my $path = shift;
open(my $fh, "<", $path) or die $!;
local $/;
my $text = <$fh>;
close($fh);
$text =~ s/<<<<<<< HEAD\n(.*?)=======\n.*?>>>>>>> origin\/main\n/$1/sg;
open(my $out, ">", $path) or die $!;
print $out $text;
close($out);
'