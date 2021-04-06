from __future__ import print_function
from io import BytesIO
import sys
import tarfile

stdin = getattr(sys.stdin, 'buffer', sys.stdin)
size = int(stdin.readline())
raw_tar = stdin.read(size)
tar = tarfile.open(fileobj=BytesIO(raw_tar), mode='r|')
modules = {}
for f in tar:
    content = tar.extractfile(f).read()
    modules['cinnabar/' + f.name] = content

del tar
del raw_tar
del stdin
