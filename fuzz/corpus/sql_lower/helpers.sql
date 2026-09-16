SELECT IFNULL(name, 'x'), json_extract(payload, '$.k') FROM t
