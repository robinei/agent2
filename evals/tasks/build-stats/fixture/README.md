# stats.py

A one-file command-line tool. Nothing is installed here, so it may use
only the Python standard library.

    python3 stats.py <path-to-csv>

The CSV has a header row. A column counts as **numeric** when every one
of its non-empty cells parses as a float; empty cells are ignored. A
column with no non-empty cells is not numeric.

Print one line per numeric column, in the order the columns appear in
the file, and nothing else:

    name,min,max,mean

`min`, `max` and `mean` are each formatted to exactly two decimal
places. Non-numeric columns are not printed.

`example.csv` and `expected.txt` are here so you can check yourself:
running the tool on the first must print exactly the second.
