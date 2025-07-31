'use strict';

class SqliteError extends Error {
  constructor(message, code, rawCode) {
    super(message);
    this.name    = 'SqliteError';
    this.code    = code;
    this.rawCode = rawCode;

    Error.captureStackTrace(this, SqliteError);
  }
}

module.exports = SqliteError;
