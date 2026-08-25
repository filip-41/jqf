#!/usr/bin/env perl
#
# jqf-timeout-run.pl <seconds> <pidfile> -- <command...>
#
# Runs <command...> in a NEW PROCESS GROUP and kills the whole group when the
# bound expires. This is the ladder's hang guard: hyperfine has no --timeout
# (1.20.0 rejects the flag outright), and SIGTERM to hyperfine alone orphans
# its benchmarked children (verified: a hung child is reparented to PID 1 and
# keeps running). Killing the process group — which the timed command's
# descendants inherit — is the only way to actually stop a hung lane.
#
# Exit status: the command's own status, or 124 when the bound expired.
# On timeout the group is KILLed (SIGKILL), because a TERM'd group can hang
# again inside its own signal handler.
#
# Perl rather than a shell background-sleep-kill loop because bash has no
# portable "wait until PID exits OR deadline" that also reaps the child; the
# POSIX alarm + waitpid pair in this file is exactly that.
#
# Usage from a script:
#   tools/jqf-timeout-run.pl 60 "$outdir/lane.probe.pid" -- "$cmd"
#
# The pidfile receives the GROUP LEADER's pid (the direct child); the caller
# may ignore it. If the pidfile cannot be written the command still runs.

use strict;
use warnings;
use POSIX ();

my $secs   = shift @ARGV;
my $pidfile = shift @ARGV;
die "usage: jqf-timeout-run.pl <seconds> <pidfile> -- <command...>\n"
    unless defined $secs && $secs =~ /^\d+$/ && defined $pidfile && @ARGV && $ARGV[0] eq '--';
shift @ARGV;   # the "--"

my $pid = fork();
die "fork: $!\n" unless defined $pid;

if ($pid == 0) {
    # Child: become the leader of a new process group, then exec.
    POSIX::setpgid(0, 0);
    exec @ARGV;
    exit 127;
}

if (open my $pf, ">", $pidfile) {
    print {$pf} "$pid\n";
    close $pf;
}

my $timed_out = 0;
eval {
    local $SIG{ALRM} = sub { die "TIMEOUT\n" };
    alarm $secs;
    waitpid($pid, 0);
    alarm 0;
};
if ($@) {
    $timed_out = 1;
    # Negative pid kills the whole process group (SIGKILL: no handler can
    # swallow it, no child can survive to hang a second time).
    kill 'KILL', -$pid;
    waitpid($pid, 0);
}

exit 124 if $timed_out;
exit($? >> 8);
